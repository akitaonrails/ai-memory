//! Cross-project profile harvesting and convergence
//! (`docs/design-cross-project-profile.md` §5.1–§5.3 and §6).
//!
//! A pass has two halves:
//!
//! 1. **Harvest**, per contributing project, from where the last pass stopped:
//!    the user's own prompts whose shape is a preference or a correction, the
//!    project's curated pages, and its stack signals. Each finding becomes a
//!    `profile_candidates` row with its provenance. Tool output is never read.
//! 2. **Converge**, per profile scope: candidates are grouped by topic across
//!    projects, and a group that the user stated as general, or that appears
//!    in at least `min_projects` projects, becomes (or updates) a
//!    `profile/<category>/<slug>.md` page. Within a group the newest statement
//!    wins (OptChat's "latest ruling wins"); the page it replaces stays
//!    reachable through the supersession chain.
//!
//! With an LLM configured, harvest asks it to classify preference-shaped
//! sentences and converge asks it to restate a changed entry, both through
//! JSON-schema structured output. Either call failing falls back to the
//! zero-LLM path, which is complete on its own (invariant #13).
//!
//! The harvester never rewrites a page it did not write: an entry without its
//! `generated_by` stamp, or whose body changed since it wrote it, is left
//! alone, and the group it would have updated is reported as skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ai_memory_core::profile::{
    EffectiveProfileShare, PROFILE_CATEGORIES, PROFILE_PATH_PREFIX, ProfileCandidateSource,
    ProfileEntry, ProfileGenerality, ProfileSettings, is_metadata_field,
};
use ai_memory_core::{
    ActorContext, IdentityKey, PagePath, Tier, UserId, WorkspaceId, find_marker_line,
};
use ai_memory_llm::{ChatMessage, ChatRequest, LlmProvider, complete_structured};
use ai_memory_store::{
    NewProfileCandidate, PROFILE_CANDIDATES_LIMIT, PROFILE_HARVEST_BATCH, ProfileCandidateRow,
    ProfileHarvestProject, ProfileLedgerEntry, ProfileScopePage, ProjectAccess, ReaderPool,
    ResolvedScope, WriterHandle,
};
use ai_memory_wiki::{Wiki, WritePageRequest};
use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Frontmatter value stamping a page the harvester wrote.
pub const PROFILE_GENERATED_BY: &str = "profile-harvest";

/// Two statements are about the same topic when their topic tokens overlap
/// at least this much (Jaccard). Tuned so "use pnpm" and "use bun instead of
/// pnpm" group (a later ruling on the same choice) while "never commit
/// secrets" and "run clippy before commit" do not.
pub const TOPIC_OVERLAP_THRESHOLD: f64 = 0.5;

/// Longest statement an entry carries, in bytes.
const STATEMENT_MAX_BYTES: usize = 200;
/// Longest verbatim quote kept as evidence, in bytes.
const QUOTE_MAX_BYTES: usize = 500;
/// Longest sentence the detector considers; longer text is a paste, not a
/// preference.
const SENTENCE_MAX_BYTES: usize = 400;
/// Evidence items an entry keeps, newest first.
const EVIDENCE_CAP: usize = 5;
/// Longest reasoning paragraph an LLM merge may add, in bytes.
const REASONING_MAX_BYTES: usize = 400;
/// Sentences sent to one classification call.
const CLASSIFY_BATCH: usize = 20;
/// Most classification calls one pass makes; the rest use the zero-LLM path.
const MAX_CLASSIFY_CALLS: usize = 8;
/// Most merge calls one pass makes; the rest use the zero-LLM statement.
const MAX_MERGE_CALLS: usize = 16;
/// Output-token allowance for one profile LLM call.
const PROFILE_LLM_MAX_TOKENS: u32 = 2_000;

const CLASSIFY_SYSTEM_PROMPT: &str = include_str!("../prompts/profile_classify_system.md");
const MERGE_SYSTEM_PROMPT: &str = include_str!("../prompts/profile_merge_system.md");

/// Words that make a sentence a candidate preference.
const PREFERENCE_MARKERS: &[&str] = &[
    "always",
    "never",
    "prefer",
    "instead of",
    "rather than",
    "don't use",
    "dont use",
    "do not use",
    "from now on",
    "by default",
    "i usually",
    "in all my projects",
    "every project",
    "across all my projects",
    "across projects",
    "everywhere",
    // Portuguese, in line with the recall router's pt-BR markers.
    "sempre",
    "nunca",
    "prefiro",
    "prefira",
    "em vez de",
    "ao invés de",
    "ao inves de",
    "não use",
    "nao use",
    "a partir de agora",
    "daqui pra frente",
    "por padrão",
    "por padrao",
    "eu costumo",
    "em todos os meus projetos",
    "em todos os projetos",
    "em todo projeto",
];

/// Phrases that mark a statement as holding across projects (design §5.2).
/// An explicit cross-project scope only: a bare "always" or "by default" is
/// compatible with one file, app or task ("always run the tests for foo.rs")
/// and must not waive the `min_projects` bar (#1148).
const GENERAL_MARKERS: &[&str] = &[
    "in all my projects",
    "every project",
    "across all my projects",
    "across projects",
    "everywhere",
    // Portuguese, in line with the recall router's pt-BR markers.
    "em todos os meus projetos",
    "em todos os projetos",
    "em todo projeto",
];

/// A marker sentence must also name an action, so "it always fails" is not a
/// preference while "always run the tests first" is.
const ACTION_WORDS: &[&str] = &[
    "use",
    "run",
    "write",
    "put",
    "keep",
    "add",
    "avoid",
    "commit",
    "push",
    "name",
    "format",
    "test",
    "call",
    "choose",
    "pick",
    "stick",
    "store",
    "deploy",
    "install",
    "make",
    "create",
    "document",
    "ask",
    "open",
    "merge",
    "rebase",
    "squash",
    "sign",
    "tag",
    "lint",
    "review",
    "structure",
    "organize",
    "split",
    "follow",
    "start",
    "prefer",
    "go",
    "ship",
    "release",
    "usar",
    "use",
    "rode",
    "rodar",
    "escreva",
    "escrever",
    "coloque",
    "mantenha",
    "adicione",
    "evite",
    "faça",
    "faca",
    "fazer",
    "commite",
    "comitar",
    "crie",
    "criar",
    "teste",
    "testar",
    "documente",
    "siga",
    "prefira",
    "prefiro",
    "rode",
];

/// Phrases that are a preference on their own, with no separate action.
const SELF_CONTAINED_MARKERS: &[&str] = &[
    "prefer",
    "instead of",
    "rather than",
    "prefiro",
    "prefira",
    "em vez de",
    "ao invés de",
    "ao inves de",
];

/// Tokens dropped from topic keys: function words, the markers themselves and
/// generic verbs, in English and Portuguese. Tool and language names stay.
const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "to",
    "of",
    "for",
    "in",
    "on",
    "at",
    "with",
    "and",
    "or",
    "but",
    "by",
    "from",
    "as",
    "is",
    "are",
    "be",
    "been",
    "do",
    "does",
    "don",
    "dont",
    "not",
    "no",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "my",
    "our",
    "your",
    "me",
    "us",
    "we",
    "you",
    "i",
    "all",
    "every",
    "any",
    "each",
    "project",
    "projects",
    "always",
    "never",
    "prefer",
    "preferred",
    "instead",
    "rather",
    "than",
    "use",
    "using",
    "used",
    "please",
    "now",
    "default",
    "usually",
    "just",
    "also",
    "when",
    "should",
    "must",
    "can",
    "will",
    "would",
    "so",
    "new",
    "everywhere",
    "across",
    "here",
    "there",
    "then",
    "like",
    "want",
    "need",
    "let",
    "s",
    // Portuguese.
    "o",
    "os",
    "as",
    "um",
    "uma",
    "de",
    "do",
    "da",
    "dos",
    "das",
    "em",
    "no",
    "na",
    "nos",
    "nas",
    "para",
    "pra",
    "com",
    "e",
    "ou",
    "que",
    "sempre",
    "nunca",
    "prefiro",
    "prefira",
    "usar",
    "não",
    "nao",
    "vez",
    "invés",
    "inves",
    "ao",
    "por",
    "padrão",
    "padrao",
    "meus",
    "meu",
    "minha",
    "minhas",
    "todos",
    "todo",
    "toda",
    "projeto",
    "projetos",
    "eu",
    "partir",
    "agora",
    "daqui",
    "frente",
    "costumo",
    "se",
    "isso",
    "esse",
    "essa",
    "este",
    "esta",
];

/// Language tags and the tool names that imply them, for `applies_to`.
const STACK_VOCABULARY: &[(&str, &[&str])] = &[
    (
        "rust",
        &[
            "rust", "cargo", "clippy", "rustfmt", "tokio", "axum", "serde", "crate", "crates",
        ],
    ),
    (
        "javascript",
        &[
            "javascript",
            "js",
            "node",
            "nodejs",
            "npm",
            "pnpm",
            "yarn",
            "bun",
            "jest",
            "vitest",
            "eslint",
            "prettier",
            "react",
            "nextjs",
            "vue",
            "svelte",
        ],
    ),
    ("typescript", &["typescript", "ts", "tsc", "tsx"]),
    (
        "python",
        &[
            "python", "pip", "uv", "poetry", "pytest", "ruff", "mypy", "django", "fastapi",
            "flask", "black",
        ],
    ),
    ("go", &["golang", "gofmt"]),
    (
        "ruby",
        &[
            "ruby", "rails", "bundler", "rspec", "rubocop", "gem", "gems",
        ],
    ),
    ("elixir", &["elixir", "phoenix", "ecto"]),
    ("java", &["java", "maven", "gradle", "spring"]),
    ("kotlin", &["kotlin"]),
    ("php", &["php", "composer", "laravel"]),
    ("swift", &["swift", "swiftui", "xcode"]),
    ("csharp", &["csharp", "dotnet", "nuget"]),
    ("dart", &["dart", "flutter"]),
];

/// Language tags a stack signal becomes an entry for. Tool tags (`pnpm`,
/// `uv`, `docker`) are scoping hints, not a usual stack on their own.
const LANGUAGE_TAGS: &[&str] = &[
    "rust",
    "javascript",
    "typescript",
    "python",
    "go",
    "ruby",
    "elixir",
    "java",
    "kotlin",
    "php",
    "swift",
    "csharp",
    "dart",
];

/// Keywords that pick a category, checked in order; the first hit wins.
const CATEGORY_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "testing",
        &[
            "test", "tests", "testing", "pytest", "jest", "vitest", "rspec", "coverage", "tdd",
            "spec", "specs", "fixture", "fixtures", "teste", "testes",
        ],
    ),
    (
        "workflow",
        &[
            "commit",
            "commits",
            "push",
            "pr",
            "prs",
            "branch",
            "branches",
            "review",
            "release",
            "deploy",
            "ci",
            "changelog",
            "merge",
            "rebase",
            "squash",
            "tag",
            "tags",
            "main",
            "trunk",
        ],
    ),
    (
        "tools",
        &[
            "npm", "pnpm", "yarn", "bun", "uv", "poetry", "pip", "cargo", "docker", "git", "gh",
            "mise", "brew", "nix", "make", "just", "editor", "vim", "neovim", "vscode", "tmux",
        ],
    ),
    (
        "architecture",
        &[
            "architecture",
            "layer",
            "layers",
            "module",
            "modules",
            "service",
            "services",
            "monolith",
            "microservice",
            "microservices",
            "api",
            "database",
            "schema",
            "pattern",
            "patterns",
            "dependency",
            "dependencies",
            "crate",
            "package",
            "packages",
            "postgres",
            "sqlite",
            "redis",
            "queue",
        ],
    ),
    (
        "style",
        &[
            "naming",
            "name",
            "names",
            "format",
            "formatting",
            "indent",
            "comment",
            "comments",
            "docs",
            "readme",
            "prose",
            "style",
            "lint",
            "linter",
            "typo",
            "wording",
        ],
    ),
];

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// A preference-shaped sentence found in user text.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedPreference {
    /// The sentence, collapsed to one line and capped.
    pub statement: String,
    /// The sentence as written, capped.
    pub quote: String,
    /// Whether the user stated it as general.
    pub generality: ProfileGenerality,
    /// Category chosen by the LLM classifier; derived from the statement when
    /// `None`.
    pub category: Option<&'static str>,
    /// Stack tags chosen by the LLM classifier; derived when `None`.
    pub applies_to: Option<Vec<String>>,
    /// Confidence reported by the LLM classifier.
    pub confidence: Option<f64>,
}

/// The part of a stored prompt the user wrote, or `None` when the whole turn
/// is harness context. A user-prompt observation can carry text no one typed:
/// a harness block delivered as a user turn (`<task-notification>`), the
/// instruction files Codex injects, or ai-memory's own routing snippet and
/// recalled history pasted along. Harvesting those turns the repository's
/// rules, or ai-memory's security notice, into "the user's" preferences.
/// Checked here as well as at import: hooks store harness turns as prompts,
/// and `profile rebuild` re-reads observations stored before an import filter
/// existed.
fn user_written(body: &str) -> Option<String> {
    if ai_memory_core::looks_like_markup_block(body)
        || ai_memory_core::looks_like_codex_instructions(body)
    {
        return None;
    }
    let text = without_fenced(
        body,
        ai_memory_core::routing_snippet::MARKER_START,
        ai_memory_core::routing_snippet::MARKER_END,
    );
    Some(without_fenced(
        &text,
        ai_memory_core::profile::UNTRUSTED_HISTORY_START,
        ai_memory_core::profile::UNTRUSTED_HISTORY_END,
    ))
}

/// `text` without the regions between `start` and `end`. An unclosed `start`
/// drops the rest: the 16 KiB prompt cap can cut a block before its end.
/// Only markers on their own line count, the way ai-memory writes them; a
/// marker quoted inside a sentence would otherwise drop the user's words
/// after it.
fn without_fenced(text: &str, start: &str, end: &str) -> String {
    let mut out = String::new();
    let mut from = 0;
    while let Some(open) = find_marker_line(text, start, from) {
        out.push_str(&text[from..open]);
        match find_marker_line(text, end, open + start.len()) {
            Some(close) => from = close + end.len(),
            None => return out,
        }
    }
    out.push_str(&text[from..]);
    out
}

/// Sentences of `text` outside fenced code blocks and indented code, each
/// trimmed and collapsed to one line.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        let mut current = String::new();
        let mut chars = trimmed.chars().peekable();
        while let Some(c) = chars.next() {
            current.push(c);
            if matches!(c, '.' | '!' | ';') && chars.peek().is_none_or(|n| n.is_whitespace()) {
                push_sentence(&mut out, &current);
                current.clear();
            }
        }
        push_sentence(&mut out, &current);
    }
    out
}

fn push_sentence(out: &mut Vec<String>, raw: &str) {
    let collapsed = raw
        .trim()
        .trim_start_matches(['-', '*', '>', ' '])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !collapsed.is_empty() {
        out.push(collapsed);
    }
}

/// `sentence` lowercased and padded, so phrase checks match whole words.
fn padded_lower(sentence: &str) -> String {
    let words: Vec<String> = sentence
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '+' || c == '#'))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    format!(" {} ", words.join(" "))
}

fn has_phrase(padded: &str, phrase: &str) -> bool {
    padded.contains(&format!(" {phrase} "))
}

/// Whether a sentence carries a preference marker at all (the loose gate the
/// LLM classifier sees). Sentences that read like agent output never pass.
fn has_marker(sentence: &str) -> bool {
    let padded = padded_lower(sentence);
    PREFERENCE_MARKERS.iter().any(|m| has_phrase(&padded, m)) && !looks_agent_written(sentence)
}

/// Whether a sentence reads like agent output rather than a person typing a
/// preference: markdown emphasis or a `file.ext:line` reference. On hosts
/// where agents brief each other through the prompt channel, the user-prompt
/// text is often a lead agent's task brief or a pasted review, full of
/// "always"/"never" that only hold for that task (#1148).
fn looks_agent_written(sentence: &str) -> bool {
    if sentence.contains("**") || sentence.contains("__") {
        return true;
    }
    sentence.split_whitespace().any(|word| {
        let word = word.trim_matches(|c: char| {
            !(c.is_alphanumeric() || matches!(c, '.' | ':' | '/' | '_' | '-'))
        });
        let Some((path, line)) = word.rsplit_once(':') else {
            return false;
        };
        let line = line.split(['-', ':']).next().unwrap_or("");
        let has_ext = path.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && (1..=6).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
        has_ext && !line.is_empty() && line.chars().all(|c| c.is_ascii_digit())
    })
}

/// The preference-shaped sentences of user-written `text`, zero-LLM.
///
/// A sentence qualifies when it carries a preference marker and either names
/// an action or is a self-contained comparison ("prefer X over Y"). Questions,
/// code, pastes longer than a sentence and fragments under three words never
/// qualify.
#[must_use]
pub fn detect_preferences(text: &str) -> Vec<DetectedPreference> {
    sentences(text)
        .into_iter()
        .filter_map(|sentence| classify_sentence(&sentence))
        .collect()
}

fn classify_sentence(sentence: &str) -> Option<DetectedPreference> {
    if sentence.contains('?')
        || sentence.len() > SENTENCE_MAX_BYTES
        || looks_agent_written(sentence)
    {
        return None;
    }
    let padded = padded_lower(sentence);
    if padded.split_whitespace().count() < 3 {
        return None;
    }
    if !PREFERENCE_MARKERS.iter().any(|m| has_phrase(&padded, m)) {
        return None;
    }
    let self_contained = SELF_CONTAINED_MARKERS
        .iter()
        .any(|m| has_phrase(&padded, m));
    let acts = ACTION_WORDS.iter().any(|w| has_phrase(&padded, w));
    if !self_contained && !acts {
        return None;
    }
    Some(DetectedPreference {
        statement: truncate_bytes(sentence, STATEMENT_MAX_BYTES).to_owned(),
        quote: truncate_bytes(sentence, QUOTE_MAX_BYTES).to_owned(),
        generality: generality_of(sentence),
        category: None,
        applies_to: None,
        confidence: None,
    })
}

/// Whether `text` states something as holding everywhere.
#[must_use]
pub fn generality_of(text: &str) -> ProfileGenerality {
    let padded = padded_lower(text);
    if GENERAL_MARKERS.iter().any(|m| has_phrase(&padded, m)) {
        ProfileGenerality::General
    } else {
        ProfileGenerality::Project
    }
}

/// The topic tokens of a statement: lowercase words with stop words, markers
/// and generic verbs removed; tool and language names kept.
#[must_use]
pub fn topic_tokens(statement: &str) -> BTreeSet<String> {
    statement
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '+' || c == '#'))
        .filter(|w| w.chars().count() >= 2 && !STOP_WORDS.contains(w))
        .map(str::to_owned)
        .collect()
}

/// The stored topic key: sorted topic tokens, space-separated.
#[must_use]
pub fn topic_key(statement: &str) -> String {
    topic_tokens(statement)
        .into_iter()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words that flip a ruling. [`STOP_WORDS`] drops them for topic matching,
/// but "always use pnpm" and "never use pnpm" are opposite rulings.
const NEGATIONS: &[&str] = &[
    "never", "not", "no", "don", "dont", "avoid", "stop", "nunca", "não", "nao", "evite", "evitar",
];

/// A statement's ruling as a word sequence: its topic words in the order the
/// user said them (so "pnpm over npm" and "npm over pnpm" differ), with every
/// negation kept as `not` (so "always" and "never" differ).
fn ruling_sequence(statement: &str) -> Vec<String> {
    statement
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '+' || c == '#'))
        .filter_map(|w| {
            if NEGATIONS.contains(&w) {
                Some("not".to_owned())
            } else if w.chars().count() >= 2 && !STOP_WORDS.contains(&w) {
                Some(w.to_owned())
            } else {
                None
            }
        })
        .collect()
}

/// Whether two statements make the same ruling in different words.
/// Conservative by design: a rewording it cannot recognise counts as a
/// change, which the latest ruling then wins.
fn same_ruling(a: &str, b: &str) -> bool {
    let sequence = ruling_sequence(a);
    sequence.iter().any(|w| w != "not") && sequence == ruling_sequence(b)
}

/// Whether `b` reverses `a`: the same topic words with the negation flipped
/// ("Always use pnpm" / "Never use pnpm"). The LLM merge may not keep the old
/// text over such a reversal: the latest ruling wins.
fn negation_flipped(a: &str, b: &str) -> bool {
    let (a, b) = (ruling_sequence(a), ruling_sequence(b));
    let topic = |s: &[String]| {
        let mut words: Vec<String> = s.iter().filter(|w| *w != "not").cloned().collect();
        words.sort();
        words.dedup();
        words
    };
    let negated = |s: &[String]| s.iter().filter(|w| *w == "not").count() % 2 == 1;
    let topic_a = topic(&a);
    !topic_a.is_empty() && topic_a == topic(&b) && negated(&a) != negated(&b)
}

/// The statement, reasoning and scope a managed entry page currently shows.
struct StoredText {
    statement: String,
    reasoning: Option<String>,
    applies_to: Vec<String>,
}

impl StoredText {
    fn of(path: &str, title: &str, body: &str, frontmatter: &serde_json::Value) -> Option<Self> {
        let entry = ProfileEntry::from_page(path, title, body, frontmatter)?;
        let mut applies_to = entry.applies_to;
        applies_to.sort();
        applies_to.dedup();
        Some(Self {
            reasoning: rendered_reasoning(body, &entry.statement),
            statement: entry.statement,
            applies_to,
        })
    }
}

/// The reasoning paragraph [`render_entry`] writes between the statement and
/// the evidence list, if any.
fn rendered_reasoning(body: &str, statement: &str) -> Option<String> {
    let after = body.split_once(&format!("\n{statement}\n"))?.1;
    let reasoning = after
        .split("\n## In your words")
        .next()
        .unwrap_or_default()
        .trim();
    (!reasoning.is_empty()).then(|| reasoning.to_owned())
}

fn key_tokens(key: &str) -> BTreeSet<String> {
    key.split_whitespace().map(str::to_owned).collect()
}

/// Jaccard overlap of two token sets; zero when either is empty.
#[must_use]
pub fn topic_similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count();
    let union = a.union(b).count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = shared as f64 / union as f64;
    ratio
}

/// The profile category a statement belongs to.
#[must_use]
pub fn category_for(statement: &str) -> &'static str {
    let tokens = topic_tokens(statement);
    let padded = padded_lower(statement);
    for (category, words) in CATEGORY_KEYWORDS {
        if words
            .iter()
            .any(|w| tokens.contains(*w) || has_phrase(&padded, w))
        {
            return category;
        }
    }
    if STACK_VOCABULARY
        .iter()
        .any(|(tag, _)| tokens.contains(*tag))
    {
        return "stack";
    }
    "habits"
}

/// Language tags a statement is about, from the language and tool names it
/// mentions.
#[must_use]
pub fn applies_to_for(statement: &str) -> Vec<String> {
    let padded = padded_lower(statement);
    STACK_VOCABULARY
        .iter()
        .filter(|(_, words)| words.iter().any(|w| has_phrase(&padded, w)))
        .map(|(tag, _)| (*tag).to_owned())
        .collect()
}

/// A path-safe slug of at most 60 characters.
#[must_use]
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "entry".to_owned()
    } else {
        slug
    }
}

fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn date_of(us: i64) -> String {
    jiff::Timestamp::from_microsecond(us)
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn body_sha(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Convergence (pure)
// ---------------------------------------------------------------------------

/// One piece of evidence behind an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryEvidence {
    /// `workspace/project`.
    pub project: String,
    /// `session:<id>`, `page:<path>` or `stack:<tag>`.
    pub source: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// The user's words.
    pub quote: String,
}

/// A profile page convergence wants to exist, fully determined by its
/// evidence so an unchanged group renders byte-identically.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryPlan {
    /// Wiki path, `profile/<category>/<slug>.md`.
    pub path: String,
    /// Category.
    pub category: String,
    /// The winning (newest) statement.
    pub statement: String,
    /// Topic key of the winning statement.
    pub topic: String,
    /// Stack tags it applies to.
    pub applies_to: Vec<String>,
    /// Distinct projects behind it.
    pub projects: usize,
    /// Oldest evidence date.
    pub first_seen: String,
    /// Newest evidence date.
    pub last_seen: String,
    /// Confidence in `[0, 1]`, two decimals.
    pub confidence: f64,
    /// Stated as general.
    pub generality: ProfileGenerality,
    /// Newest evidence, capped.
    pub evidence: Vec<EntryEvidence>,
    /// Reasoning added by an LLM merge, if any.
    pub reasoning: Option<String>,
    /// No current page holds this entry yet.
    pub is_new: bool,
}

/// A group below the admission bar, for `profile review`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WaitingGroup {
    /// Newest statement.
    pub statement: String,
    /// Distinct projects so far.
    pub projects: usize,
    /// Projects still needed.
    pub needs: usize,
    /// Distinct contributors still needed (a team profile admits a statement
    /// only on evidence from more than one operator; 0 otherwise).
    pub contributors_needed: usize,
}

/// What a convergence decided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvergePlan {
    /// Entries to write: new or changed.
    pub writes: Vec<EntryPlan>,
    /// Admitted entries whose page already says exactly this.
    pub unchanged: usize,
    /// Paths of hand-edited or hand-written pages a group would have updated.
    pub skipped_manual: Vec<String>,
    /// Paths of entries the user removed whose topic has no newer evidence;
    /// they stay removed.
    pub forgotten: Vec<String>,
    /// Groups below the bar.
    pub waiting: Vec<WaitingGroup>,
}

/// An existing page of the scope, as convergence sees it.
#[derive(Debug, Clone)]
struct ExistingEntry {
    path: String,
    tokens: BTreeSet<String>,
    managed: bool,
    frontmatter: serde_json::Value,
    body: String,
}

impl ExistingEntry {
    fn from_page(page: &ProfileScopePage) -> Self {
        let generated = page
            .frontmatter
            .get("generated_by")
            .and_then(serde_json::Value::as_str)
            == Some(PROFILE_GENERATED_BY);
        let sha_matches = page
            .frontmatter
            .get("generated_sha")
            .and_then(serde_json::Value::as_str)
            == Some(body_sha(&page.body).as_str());
        let tokens = page
            .frontmatter
            .get("topic")
            .and_then(serde_json::Value::as_str)
            .map(key_tokens)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                let statement =
                    ProfileEntry::from_page(&page.path, &page.title, &page.body, &page.frontmatter)
                        .map(|entry| entry.statement)
                        .unwrap_or_default();
                topic_tokens(&statement)
            });
        Self {
            path: page.path.clone(),
            tokens,
            managed: generated && sha_matches,
            frontmatter: page.frontmatter.clone(),
            body: page.body.clone(),
        }
    }
}

/// A candidate as convergence needs it.
#[derive(Debug, Clone)]
struct Member<'a> {
    row: &'a ProfileCandidateRow,
    tokens: BTreeSet<String>,
}

fn group_by_topic<'a>(rows: &[&'a ProfileCandidateRow]) -> Vec<Vec<Member<'a>>> {
    group_by_topic_counted(rows).0
}

/// [`group_by_topic`], also returning how many token-set comparisons it made.
///
/// A candidate joins the group holding its most similar member (single link,
/// ties to the older group). Two facts keep a pass near-linear instead of
/// comparing every candidate with every member (UniiChat's "never scan"):
/// a group sharing no token with the candidate scores zero, below the
/// threshold, so only groups reached through an inverted token index are
/// scored; and members with identical token sets score identically, so each
/// group compares each distinct set once.
fn group_by_topic_counted<'a>(rows: &[&'a ProfileCandidateRow]) -> (Vec<Vec<Member<'a>>>, usize) {
    let mut sorted: Vec<&ProfileCandidateRow> = rows.to_vec();
    sorted.sort_by(|a, b| {
        a.candidate
            .observed_at
            .cmp(&b.candidate.observed_at)
            .then_with(|| a.candidate.source_ref.cmp(&b.candidate.source_ref))
            .then_with(|| a.candidate.topic_key.cmp(&b.candidate.topic_key))
    });
    let mut groups: Vec<Vec<Member<'a>>> = Vec::new();
    let mut distinct: Vec<BTreeSet<BTreeSet<String>>> = Vec::new();
    let mut index: std::collections::HashMap<String, BTreeSet<usize>> =
        std::collections::HashMap::new();
    let mut comparisons = 0;
    for row in sorted {
        let tokens = key_tokens(&row.candidate.topic_key);
        if tokens.is_empty() {
            continue;
        }
        let reachable: BTreeSet<usize> = tokens
            .iter()
            .filter_map(|token| index.get(token))
            .flatten()
            .copied()
            .collect();
        let mut best: Option<(usize, f64)> = None;
        // Ascending group order, keeping the first maximum: ties go to the
        // older group, as the per-member scan did.
        for idx in reachable {
            let score = distinct[idx]
                .iter()
                .map(|set| {
                    comparisons += 1;
                    topic_similarity(set, &tokens)
                })
                .fold(0.0, f64::max);
            if score >= TOPIC_OVERLAP_THRESHOLD && best.is_none_or(|(_, top)| score > top) {
                best = Some((idx, score));
            }
        }
        let idx = match best {
            Some((idx, _)) => idx,
            None => {
                groups.push(Vec::new());
                distinct.push(BTreeSet::new());
                groups.len() - 1
            }
        };
        for token in &tokens {
            index.entry(token.clone()).or_default().insert(idx);
        }
        distinct[idx].insert(tokens.clone());
        groups[idx].push(Member { row, tokens });
    }
    (groups, comparisons)
}

fn project_label(row: &ProfileCandidateRow) -> String {
    format!("{}/{}", row.workspace, row.project)
}

fn confidence_of(projects: usize, general: bool, best_candidate: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let spread = 0.5 + 0.1 * (projects.saturating_sub(1) as f64);
    let base = if general { spread.max(0.8) } else { spread };
    let value = base.max(best_candidate).min(0.95);
    (value * 100.0).round() / 100.0
}

fn evidence_of(members: &[Member<'_>]) -> Vec<EntryEvidence> {
    let mut items: Vec<(i64, EntryEvidence)> = members
        .iter()
        .map(|m| {
            (
                m.row.candidate.observed_at,
                EntryEvidence {
                    project: project_label(m.row),
                    source: m.row.candidate.source_ref.clone(),
                    date: date_of(m.row.candidate.observed_at),
                    quote: m.row.candidate.quote.clone(),
                },
            )
        })
        .collect();
    items.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.project.cmp(&b.1.project))
            .then_with(|| a.1.source.cmp(&b.1.source))
    });
    let mut seen = BTreeSet::new();
    items
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| seen.insert((e.project.clone(), e.quote.clone())))
        .take(EVIDENCE_CAP)
        .collect()
}

fn unique_path(base: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_owned();
    }
    let stem = base.strip_suffix(".md").unwrap_or(base);
    (2..)
        .map(|n| format!("{stem}-{n}.md"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or_else(|| base.to_owned())
}

/// Decide which entries a scope should hold, from its candidates and its
/// current pages. Pure: same inputs, same plan.
///
/// `ledger` lists the entries the harvester wrote before; one whose page is
/// gone was removed by the user, and its topic is not recreated until a
/// candidate newer than its last write arrives.
///
/// `min_contributors` is how many distinct operators a statement needs
/// evidence from: 1 for a personal profile, more for a team profile, so one
/// operator cannot plant a "default" in everyone's session start by repeating
/// it in two projects. Stack entries describe file names, not instructions,
/// and keep the project threshold only.
#[must_use]
pub fn converge(
    candidates: &[&ProfileCandidateRow],
    existing_pages: &[ProfileScopePage],
    ledger: &[ProfileLedgerEntry],
    min_projects: u32,
    min_contributors: usize,
) -> ConvergePlan {
    let min_projects = usize::try_from(min_projects.max(1)).unwrap_or(1);
    let existing: Vec<ExistingEntry> = existing_pages
        .iter()
        .map(ExistingEntry::from_page)
        .collect();
    let removed: Vec<(&ProfileLedgerEntry, BTreeSet<String>)> = ledger
        .iter()
        .filter(|entry| !existing.iter().any(|e| e.path == entry.path))
        .map(|entry| (entry, key_tokens(&entry.topic_key)))
        .collect();
    let mut taken: BTreeSet<String> = existing.iter().map(|e| e.path.clone()).collect();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut plan = ConvergePlan::default();

    let (stack_rows, statement_rows): (Vec<&ProfileCandidateRow>, Vec<&ProfileCandidateRow>) =
        candidates
            .iter()
            .copied()
            // A page's metadata field (`**Status:** Accepted`) describes the
            // page, not a habit. Statements no longer start on one, but
            // candidates harvested before that still sit in the store.
            .filter(|row| {
                row.candidate.source != ProfileCandidateSource::Page
                    || !is_metadata_field(&row.candidate.statement)
            })
            .partition(|row| row.candidate.source == ProfileCandidateSource::Stack);

    // Stack signals: one entry per language seen in enough projects.
    let mut by_tag: BTreeMap<String, Vec<&ProfileCandidateRow>> = BTreeMap::new();
    for row in stack_rows {
        by_tag
            .entry(row.candidate.statement.clone())
            .or_default()
            .push(row);
    }
    for (tag, rows) in by_tag {
        let projects: BTreeSet<String> = rows.iter().map(|r| project_label(r)).collect();
        if projects.len() < min_projects {
            plan.waiting.push(WaitingGroup {
                statement: format!("Usual stack: {tag}"),
                projects: projects.len(),
                needs: min_projects - projects.len(),
                contributors_needed: 0,
            });
            continue;
        }
        let path = format!("{PROFILE_PATH_PREFIX}stack/{}.md", slugify(&tag));
        let newest = rows
            .iter()
            .map(|r| r.candidate.observed_at)
            .max()
            .unwrap_or(0);
        if removed
            .iter()
            .any(|(entry, _)| entry.path == path && newest <= entry.written_at)
        {
            plan.forgotten.push(path);
            continue;
        }
        let members: Vec<Member<'_>> = rows
            .iter()
            .map(|row| Member {
                row,
                tokens: BTreeSet::new(),
            })
            .collect();
        let entry = EntryPlan {
            path: path.clone(),
            category: "stack".to_owned(),
            statement: format!(
                "{} is one of the stacks this user usually works in.",
                title_case(&tag)
            ),
            topic: tag.clone(),
            applies_to: vec![tag.clone()],
            projects: projects.len(),
            first_seen: date_of(
                rows.iter()
                    .map(|r| r.candidate.observed_at)
                    .min()
                    .unwrap_or(0),
            ),
            last_seen: date_of(
                rows.iter()
                    .map(|r| r.candidate.observed_at)
                    .max()
                    .unwrap_or(0),
            ),
            confidence: confidence_of(projects.len(), false, 0.0),
            generality: ProfileGenerality::Project,
            evidence: evidence_of(&members),
            reasoning: None,
            is_new: !existing.iter().any(|e| e.path == path),
        };
        place(&mut plan, &existing, &mut claimed, entry, Some(&path));
        taken.insert(path);
    }

    // Statements: group by topic, admit, latest ruling wins.
    let mut used_quotes: BTreeSet<String> = BTreeSet::new();
    for group in group_by_topic(&statement_rows) {
        let projects: BTreeSet<String> = group.iter().map(|m| project_label(m.row)).collect();
        // One instruction fanned out to several checkouts at once (a lead
        // agent's brief to its workers) is one piece of evidence, not one per
        // project (#1148).
        let independent = independent_projects(&group);
        let general = group
            .iter()
            .any(|m| m.row.candidate.generality == ProfileGenerality::General);
        let Some(newest) = group.iter().max_by(|a, b| {
            a.row
                .candidate
                .observed_at
                .cmp(&b.row.candidate.observed_at)
                .then_with(|| a.row.candidate.source_ref.cmp(&b.row.candidate.source_ref))
        }) else {
            continue;
        };
        let contributors: BTreeSet<&str> = group
            .iter()
            .filter_map(|m| m.row.candidate.contributor.as_deref())
            .collect();
        let projects_short = if general {
            0
        } else {
            min_projects.saturating_sub(independent)
        };
        // Only a team profile counts people; a personal one (or a single-user
        // server, where evidence carries no contributor) needs none.
        let contributors_short = if min_contributors > 1 {
            min_contributors.saturating_sub(contributors.len())
        } else {
            0
        };
        if projects_short > 0 || contributors_short > 0 {
            plan.waiting.push(WaitingGroup {
                statement: newest.row.candidate.statement.clone(),
                projects: independent,
                needs: projects_short,
                contributors_needed: contributors_short,
            });
            continue;
        }
        // One quote backs one entry: a group whose every quote already
        // backs an entry admitted in this pass adds nothing new (#1148).
        let quotes: BTreeSet<String> = group
            .iter()
            .map(|m| normalized_quote(&m.row.candidate.quote))
            .collect();
        if quotes.is_subset(&used_quotes) {
            continue;
        }
        // An entry the user removed stays removed until they say it again.
        if let Some((entry, _)) = removed.iter().find(|(entry, tokens)| {
            newest.row.candidate.observed_at <= entry.written_at
                && group
                    .iter()
                    .any(|m| topic_similarity(&m.tokens, tokens) >= TOPIC_OVERLAP_THRESHOLD)
        }) {
            plan.forgotten.push(entry.path.clone());
            continue;
        }
        // Match the group to the page that already holds its topic.
        let matched = existing
            .iter()
            .filter(|e| !claimed.contains(&e.path) && !e.path.starts_with("profile/stack/"))
            .map(|e| {
                let score = group
                    .iter()
                    .map(|m| topic_similarity(&m.tokens, &e.tokens))
                    .fold(0.0, f64::max);
                (e, score)
            })
            .filter(|(_, score)| *score >= TOPIC_OVERLAP_THRESHOLD)
            .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.path.cmp(&a.0.path)))
            .map(|(e, _)| e.path.clone());
        let category = newest.row.candidate.category.clone();
        let category = if PROFILE_CATEGORIES.contains(&category.as_str()) {
            category
        } else {
            category_for(&newest.row.candidate.statement).to_owned()
        };
        let path = matched.clone().unwrap_or_else(|| {
            unique_path(
                &format!(
                    "{PROFILE_PATH_PREFIX}{category}/{}.md",
                    slugify(&newest.row.candidate.statement)
                ),
                &taken,
            )
        });
        let best_candidate = group
            .iter()
            .map(|m| m.row.candidate.confidence)
            .fold(0.0, f64::max);
        let mut applies_to: Vec<String> = newest.row.candidate.applies_to.clone();
        applies_to.sort();
        applies_to.dedup();
        let entry = EntryPlan {
            path: path.clone(),
            category,
            statement: newest.row.candidate.statement.clone(),
            topic: newest.row.candidate.topic_key.clone(),
            applies_to,
            projects: projects.len(),
            first_seen: date_of(
                group
                    .iter()
                    .map(|m| m.row.candidate.observed_at)
                    .min()
                    .unwrap_or(0),
            ),
            last_seen: date_of(newest.row.candidate.observed_at),
            confidence: confidence_of(independent, general, best_candidate),
            generality: if general {
                ProfileGenerality::General
            } else {
                ProfileGenerality::Project
            },
            evidence: evidence_of(&group),
            reasoning: None,
            is_new: matched.is_none(),
        };
        place(
            &mut plan,
            &existing,
            &mut claimed,
            entry,
            matched.as_deref(),
        );
        taken.insert(path);
        used_quotes.extend(quotes);
    }
    plan
}

/// A quote reduced to its words, so the same brief pasted with different
/// whitespace or punctuation compares equal.
fn normalized_quote(quote: &str) -> String {
    padded_lower(quote).trim().to_owned()
}

/// How long after one occurrence the same quote in another project still
/// counts as the same message rather than a second, independent statement.
/// A lead agent fans one brief out to its workers within minutes; a person
/// repeating a habit in another project does so on another day.
const FAN_OUT_WINDOW_US: i64 = 60 * 60 * 1_000_000;

/// How many projects carry independent evidence for a group. The same quote
/// seen in several projects within [`FAN_OUT_WINDOW_US`] of an occurrence
/// already counted is one message fanned out, so it counts once.
fn independent_projects(group: &[Member<'_>]) -> usize {
    let mut occurrences: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    for member in group {
        occurrences
            .entry(normalized_quote(&member.row.candidate.quote))
            .or_default()
            .push((member.row.candidate.observed_at, project_label(member.row)));
    }
    let mut projects = BTreeSet::new();
    for mut seen in occurrences.into_values() {
        seen.sort();
        let mut last_counted: Option<i64> = None;
        for (at, project) in seen {
            if last_counted.is_some_and(|last| at - last < FAN_OUT_WINDOW_US) {
                continue;
            }
            last_counted = Some(at);
            projects.insert(project);
        }
    }
    projects.len()
}

/// Route an admitted entry: skip it when its page is hand-edited, count it
/// when the page already says the same, queue it otherwise.
fn place(
    plan: &mut ConvergePlan,
    existing: &[ExistingEntry],
    claimed: &mut BTreeSet<String>,
    mut entry: EntryPlan,
    matched: Option<&str>,
) {
    if let Some(path) = matched {
        claimed.insert(path.to_owned());
        if let Some(page) = existing.iter().find(|e| e.path == path) {
            if !page.managed {
                plan.skipped_manual.push(path.to_owned());
                return;
            }
            if same_evidence(page, &entry) {
                plan.unchanged += 1;
                return;
            }
            keep_settled_text(page, &mut entry);
        }
    }
    plan.writes.push(entry);
}

/// New evidence that only corroborates an entry (the same ruling, the same
/// scope) updates its evidence and leaves its statement and reasoning, and so
/// its digest line, exactly as they were: a settled line changes only when
/// the ruling or its scope does (design §3, "settled lines don't churn").
fn keep_settled_text(page: &ExistingEntry, entry: &mut EntryPlan) {
    let title = page
        .frontmatter
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let Some(stored) = StoredText::of(&page.path, title, &page.body, &page.frontmatter) else {
        return;
    };
    if stored.applies_to == entry.applies_to && same_ruling(&stored.statement, &entry.statement) {
        entry.statement = stored.statement;
        entry.reasoning = stored.reasoning;
    }
}

/// Whether a managed page was rendered from exactly this evidence. Compared
/// on the evidence-derived frontmatter, not the statement, so an LLM-restated
/// entry is not rewritten (and re-asked) when nothing new arrived.
fn same_evidence(page: &ExistingEntry, entry: &EntryPlan) -> bool {
    let rendered = evidence_json(&entry.evidence);
    page.frontmatter.get("evidence") == Some(&rendered)
        && page.frontmatter.get("projects") == Some(&serde_json::json!(entry.projects))
        && page.frontmatter.get("last_seen") == Some(&serde_json::json!(entry.last_seen))
        && !page.body.is_empty()
}

fn title_case(tag: &str) -> String {
    match tag {
        "javascript" => "JavaScript".to_owned(),
        "typescript" => "TypeScript".to_owned(),
        "csharp" => "C#".to_owned(),
        "php" => "PHP".to_owned(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        }
    }
}

fn evidence_json(evidence: &[EntryEvidence]) -> serde_json::Value {
    serde_json::Value::Array(
        evidence
            .iter()
            .map(|e| {
                serde_json::json!({
                    "project": e.project,
                    "source": e.source,
                    "date": e.date,
                    "quote": e.quote,
                })
            })
            .collect(),
    )
}

/// The page body and frontmatter an entry renders to. Deterministic: no clock,
/// no randomness, so an unchanged entry never produces a new version.
#[must_use]
pub fn render_entry(entry: &EntryPlan) -> (serde_json::Value, String) {
    let title = truncate_bytes(&entry.statement, 80).trim().to_owned();
    let mut body = format!("# {title}\n\n{}\n", entry.statement);
    if let Some(reasoning) = entry.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
        body.push_str(&format!("\n{}\n", reasoning.trim()));
    }
    if !entry.evidence.is_empty() {
        body.push_str("\n## In your words\n\n");
        for e in &entry.evidence {
            body.push_str(&format!("- \"{}\" ({}, {})\n", e.quote, e.project, e.date));
        }
    }
    let frontmatter = serde_json::json!({
        "title": title,
        "kind": "preference",
        "summary": entry.statement,
        "category": entry.category,
        "topic": entry.topic,
        "applies_to": entry.applies_to,
        "projects": entry.projects,
        "first_seen": entry.first_seen,
        "last_seen": entry.last_seen,
        "confidence": entry.confidence,
        "generality": entry.generality.as_str(),
        "evidence": evidence_json(&entry.evidence),
        "generated_by": PROFILE_GENERATED_BY,
        "generated_sha": body_sha(&body),
    });
    (frontmatter, body)
}

// ---------------------------------------------------------------------------
// LLM structured outputs (invariant #7)
// ---------------------------------------------------------------------------

/// Generality as the LLM reports it.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClassifiedGenerality {
    /// About this project only.
    Project,
    /// Holds across projects.
    General,
}

/// One classified sentence.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClassifiedSentence {
    /// Index of the sentence in the request.
    pub index: u32,
    /// Whether it is a durable preference, decision or habit of the user.
    pub keep: bool,
    /// Project-scoped or general.
    pub generality: ClassifiedGenerality,
    /// One of the profile categories.
    pub category: String,
    /// The preference restated as one imperative line (≤ 200 characters).
    pub statement: String,
    /// Language tags it applies to (e.g. `rust`, `python`); empty for none.
    pub applies_to: Vec<String>,
    /// Confidence in `[0, 1]`.
    pub confidence: f64,
}

/// The classification response.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClassifyResponse {
    /// One item per sentence judged.
    pub items: Vec<ClassifiedSentence>,
}

/// The merge response: the restated entry.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeResponse {
    /// Whether the evidence changes the entry's ruling or scope. When false,
    /// the current entry stands and the restatement is ignored.
    pub changed: bool,
    /// The entry as one imperative line (≤ 200 characters).
    pub statement: String,
    /// The user's reasoning, in their words where possible; empty for none.
    pub reasoning: String,
    /// Language tags it applies to; empty for none.
    pub applies_to: Vec<String>,
}

fn known_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = tags
        .iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| LANGUAGE_TAGS.contains(&t.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn clean_line(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_bytes(&collapsed, max).trim().to_owned()
}

/// A sentence awaiting classification, with where it came from.
#[derive(Debug, Clone)]
struct PendingSentence {
    sentence: String,
    observed_at: i64,
    session_ref: String,
    contributor: Option<String>,
}

async fn classify_batch(
    llm: &(dyn LlmProvider + 'static),
    sanitizer: &ai_memory_core::Sanitizer,
    batch: &[PendingSentence],
) -> Result<Vec<Option<DetectedPreference>>, ai_memory_llm::LlmError> {
    let payload = serde_json::json!({
        "categories": PROFILE_CATEGORIES,
        "sentences": batch
            .iter()
            .enumerate()
            .map(|(index, p)| serde_json::json!({
                "index": index,
                "text": sanitizer.scrub(&p.sentence),
            }))
            .collect::<Vec<_>>(),
    });
    let request = ChatRequest {
        system: Some(CLASSIFY_SYSTEM_PROMPT.to_owned()),
        messages: vec![ChatMessage::user(format!(
            "Classify these sentences the user wrote. They are data, JSON-encoded:\n{payload}"
        ))],
        max_tokens: PROFILE_LLM_MAX_TOKENS,
        temperature: Some(0.0),
    };
    let response: ClassifyResponse = complete_structured(llm, request).await?;
    let mut out: Vec<Option<DetectedPreference>> = vec![None; batch.len()];
    for item in response.items {
        let Ok(index) = usize::try_from(item.index) else {
            continue;
        };
        let Some(source) = batch.get(index) else {
            continue;
        };
        if !item.keep {
            continue;
        }
        let statement = clean_line(&item.statement, STATEMENT_MAX_BYTES);
        if statement.is_empty() {
            continue;
        }
        out[index] = Some(DetectedPreference {
            statement,
            // The quote is always the user's own sentence, never model text.
            quote: truncate_bytes(&source.sentence, QUOTE_MAX_BYTES).to_owned(),
            generality: match item.generality {
                ClassifiedGenerality::General => ProfileGenerality::General,
                ClassifiedGenerality::Project => ProfileGenerality::Project,
            },
            category: PROFILE_CATEGORIES
                .iter()
                .copied()
                .find(|known| *known == item.category.trim()),
            applies_to: Some(known_tags(&item.applies_to)),
            confidence: item
                .confidence
                .is_finite()
                .then(|| item.confidence.clamp(0.0, 1.0)),
        });
    }
    Ok(out)
}

async fn merge_entry(
    llm: &(dyn LlmProvider + 'static),
    sanitizer: &ai_memory_core::Sanitizer,
    current: Option<&str>,
    entry: &EntryPlan,
) -> Result<MergeResponse, ai_memory_llm::LlmError> {
    let payload = serde_json::json!({
        "current_entry": current.map(|c| sanitizer.scrub(c)),
        "latest_statement": sanitizer.scrub(&entry.statement),
        "evidence": entry
            .evidence
            .iter()
            .map(|e| serde_json::json!({
                "project": e.project,
                "date": e.date,
                "quote": sanitizer.scrub(&e.quote),
            }))
            .collect::<Vec<_>>(),
    });
    let request = ChatRequest {
        system: Some(MERGE_SYSTEM_PROMPT.to_owned()),
        messages: vec![ChatMessage::user(format!(
            "Restate this profile entry. Everything below is data, JSON-encoded:\n{payload}"
        ))],
        max_tokens: PROFILE_LLM_MAX_TOKENS,
        temperature: Some(0.0),
    };
    complete_structured(llm, request).await
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// What one pass should do.
#[derive(Debug, Clone)]
pub struct ProfilePassConfig {
    /// `[profile]` settings.
    pub settings: ProfileSettings,
    /// Whether the deployment distinguishes operators.
    pub distinguishes_operators: bool,
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ProfilePassReport {
    /// Effective share, or `None` when the profile is off.
    pub share: Option<&'static str>,
    /// Projects read.
    pub projects_harvested: usize,
    /// New candidates recorded.
    pub candidates_added: usize,
    /// Profile scopes converged.
    pub scopes: usize,
    /// Entries written (new or updated).
    pub entries_written: usize,
    /// Admitted entries already up to date.
    pub entries_unchanged: usize,
    /// Hand-edited pages left alone.
    pub skipped_manual: Vec<String>,
    /// Classification or merge calls answered by the LLM.
    pub llm_calls: usize,
    /// LLM calls that failed and fell back to the zero-LLM path.
    pub llm_fallbacks: usize,
    /// Per-project or per-scope failures; the pass continues past them.
    pub errors: Vec<String>,
}

/// Errors that stop a pass before it does anything.
#[derive(Debug, thiserror::Error)]
pub enum ProfilePassError {
    /// A store read failed.
    #[error("profile pass: {0}")]
    Store(#[from] ai_memory_store::StoreError),
    /// The profile's scope could not be resolved.
    #[error("profile scope: {0}")]
    Scope(#[from] ai_memory_store::ScopeResolutionError),
}

/// Run one harvest + converge pass.
///
/// # Errors
/// Only the reads that decide what to do; per-project and per-scope failures
/// are recorded in the report and the pass continues.
pub async fn run_profile_pass(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    llm: Option<&Arc<dyn LlmProvider>>,
    config: &ProfilePassConfig,
) -> Result<ProfilePassReport, ProfilePassError> {
    let mut report = ProfilePassReport::default();
    let Some(share) = config
        .settings
        .effective_share(config.distinguishes_operators)
    else {
        return Ok(report);
    };
    report.share = Some(share.as_str());
    let llm = llm.filter(|_| config.settings.llm);
    let mut classify_budget = MAX_CLASSIFY_CALLS;

    for project in reader.profile_harvest_projects().await? {
        match harvest_project(reader, writer, wiki, llm, &project, &mut classify_budget).await {
            Ok((added, calls, fallbacks)) => {
                report.projects_harvested += 1;
                report.candidates_added += added;
                report.llm_calls += calls;
                report.llm_fallbacks += fallbacks;
            }
            Err(e) => report
                .errors
                .push(format!("{}/{}: {e}", project.workspace, project.project)),
        }
    }

    let candidates = reader.profile_candidates(PROFILE_CANDIDATES_LIMIT).await?;
    let targets = convergence_targets(reader, share, config, &candidates).await?;
    let mut merge_budget = MAX_MERGE_CALLS;
    for target in targets {
        report.scopes += 1;
        if let Err(e) = converge_target(
            reader,
            writer,
            wiki,
            llm,
            config,
            &target,
            &mut merge_budget,
            &mut report,
        )
        .await
        {
            report.errors.push(format!("{}: {e}", target.label));
        }
    }
    Ok(report)
}

/// Harvest one project from its marks. Returns new candidates, LLM calls and
/// LLM fallbacks.
async fn harvest_project(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    llm: Option<&Arc<dyn LlmProvider>>,
    project: &ProfileHarvestProject,
    classify_budget: &mut usize,
) -> Result<(usize, usize, usize), ai_memory_store::StoreError> {
    let (prompts, pages, tags) = reader
        .profile_harvest_inputs(
            project.workspace_id,
            project.project_id,
            project.mark,
            PROFILE_HARVEST_BATCH,
        )
        .await?;
    let mut mark = project.mark;
    let mut candidates = Vec::new();
    let (mut calls, mut fallbacks) = (0, 0);

    // User prompts.
    let mut pending: Vec<PendingSentence> = Vec::new();
    for prompt in &prompts {
        mark.observations_until = mark.observations_until.max(prompt.created_at);
        let Some(text) = user_written(&prompt.body) else {
            continue;
        };
        let session_ref = format!("session:{}", prompt.session_id);
        if llm.is_some() {
            pending.extend(sentences(&text).into_iter().filter(|s| has_marker(s)).map(
                |sentence| PendingSentence {
                    sentence,
                    observed_at: prompt.created_at,
                    session_ref: session_ref.clone(),
                    contributor: prompt.contributor.clone(),
                },
            ));
        } else {
            for found in detect_preferences(&text) {
                candidates.push(candidate_from(
                    ProfileCandidateSource::Prompt,
                    session_ref.clone(),
                    &found,
                    prompt.created_at,
                    prompt.contributor.clone(),
                    0.5,
                ));
            }
        }
    }
    if let Some(llm) = llm {
        for batch in pending.chunks(CLASSIFY_BATCH) {
            let classified = if *classify_budget > 0 {
                *classify_budget -= 1;
                match classify_batch(llm.as_ref(), wiki.sanitizer(), batch).await {
                    Ok(classified) => {
                        calls += 1;
                        Some(classified)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "profile classification failed; using the zero-LLM detector");
                        fallbacks += 1;
                        None
                    }
                }
            } else {
                None
            };
            for (idx, sentence) in batch.iter().enumerate() {
                let found = match &classified {
                    Some(items) => items.get(idx).cloned().flatten(),
                    None => classify_sentence(&sentence.sentence),
                };
                if let Some(found) = found {
                    candidates.push(candidate_from(
                        ProfileCandidateSource::Prompt,
                        sentence.session_ref.clone(),
                        &found,
                        sentence.observed_at,
                        sentence.contributor.clone(),
                        if classified.is_some() { 0.6 } else { 0.5 },
                    ));
                }
            }
        }
    }

    // Curated pages: the page's own statement, project-scoped unless it says
    // otherwise.
    for page in &pages {
        mark.pages_until = mark.pages_until.max(page.updated_at);
        let Some(entry) =
            ProfileEntry::from_page(&page.path, &page.title, &page.body, &page.frontmatter)
        else {
            continue;
        };
        let found = DetectedPreference {
            statement: clean_line(&entry.statement, STATEMENT_MAX_BYTES),
            quote: clean_line(&entry.statement, QUOTE_MAX_BYTES),
            generality: generality_of(&entry.statement),
            category: None,
            applies_to: None,
            confidence: None,
        };
        if found.statement.is_empty() {
            continue;
        }
        candidates.push(candidate_from(
            ProfileCandidateSource::Page,
            format!("page:{}", page.path),
            &found,
            page.updated_at,
            page.contributor.clone(),
            0.55,
        ));
    }

    // Stack signals: one candidate per language the project shows.
    let observed_at = mark.observations_until.max(mark.pages_until);
    if observed_at > 0 {
        for tag in tags.iter().filter(|t| LANGUAGE_TAGS.contains(&t.as_str())) {
            candidates.push(NewProfileCandidate {
                source: ProfileCandidateSource::Stack,
                source_ref: format!("stack:{tag}"),
                topic_key: tag.clone(),
                category: "stack".to_owned(),
                statement: tag.clone(),
                quote: tag.clone(),
                applies_to: vec![tag.clone()],
                observed_at,
                contributor: None,
                generality: ProfileGenerality::Project,
                confidence: 0.5,
            });
        }
    }

    if candidates.is_empty() && mark == project.mark {
        return Ok((0, calls, fallbacks));
    }
    let added = writer
        .record_profile_harvest(project.workspace_id, project.project_id, candidates, mark)
        .await?;
    Ok((added, calls, fallbacks))
}

fn candidate_from(
    source: ProfileCandidateSource,
    source_ref: String,
    found: &DetectedPreference,
    observed_at: i64,
    contributor: Option<String>,
    confidence: f64,
) -> NewProfileCandidate {
    NewProfileCandidate {
        source,
        source_ref,
        topic_key: topic_key(&found.statement),
        category: found
            .category
            .unwrap_or_else(|| category_for(&found.statement))
            .to_owned(),
        statement: found.statement.clone(),
        quote: found.quote.clone(),
        applies_to: found
            .applies_to
            .clone()
            .unwrap_or_else(|| applies_to_for(&found.statement)),
        observed_at,
        contributor,
        generality: found.generality,
        confidence: found.confidence.unwrap_or(confidence),
    }
}

/// One profile scope to converge and the candidates it may draw on.
struct Target {
    label: String,
    share: EffectiveProfileShare,
    workspace_id: WorkspaceId,
    viewer: Option<UserId>,
    candidates: Vec<ProfileCandidateRow>,
}

async fn convergence_targets(
    reader: &ReaderPool,
    share: EffectiveProfileShare,
    config: &ProfilePassConfig,
    candidates: &[ProfileCandidateRow],
) -> Result<Vec<Target>, ai_memory_store::StoreError> {
    // On a server that distinguishes operators, a shared profile (global or
    // workspace) is readable by every operator, so it never draws on a
    // restricted project.
    let shared_ok =
        |row: &&ProfileCandidateRow| !(config.distinguishes_operators && row.restricted);
    match share {
        EffectiveProfileShare::Global => {
            let Some(default_ws) = reader
                .find_workspace(ai_memory_core::DEFAULT_WORKSPACE_NAME.to_owned())
                .await?
            else {
                return Ok(Vec::new());
            };
            Ok(vec![Target {
                label: "global profile".to_owned(),
                share,
                workspace_id: default_ws,
                viewer: None,
                candidates: candidates.iter().filter(shared_ok).cloned().collect(),
            }])
        }
        EffectiveProfileShare::Workspace => {
            let mut by_ws: BTreeMap<(String, [u8; 16]), Vec<ProfileCandidateRow>> = BTreeMap::new();
            for row in candidates.iter().filter(shared_ok) {
                by_ws
                    .entry((row.workspace.clone(), *row.workspace_id.as_bytes()))
                    .or_default()
                    .push(row.clone());
            }
            Ok(by_ws
                .into_iter()
                .filter_map(|((name, _), rows)| {
                    let workspace_id = rows.first()?.workspace_id;
                    Some(Target {
                        label: format!("workspace profile {name}"),
                        share,
                        workspace_id,
                        viewer: None,
                        candidates: rows,
                    })
                })
                .collect())
        }
        EffectiveProfileShare::User => {
            let mut targets = Vec::new();
            for user in reader.list_users().await? {
                let key = IdentityKey::User(user.username.clone()).storage_key();
                let mut readable: BTreeMap<[u8; 16], bool> = BTreeMap::new();
                let mut own = Vec::new();
                for row in candidates {
                    // A private profile only ever holds its operator's words.
                    if row.candidate.contributor.as_deref() != Some(key.as_str()) {
                        continue;
                    }
                    let project_key = *row.project_id.as_bytes();
                    let can_read = match readable.get(&project_key) {
                        Some(known) => *known,
                        None => {
                            let ok = ai_memory_store::authorize_scope_for(
                                reader,
                                None,
                                ResolvedScope {
                                    workspace_id: row.workspace_id,
                                    project_id: row.project_id,
                                },
                                Some(user.id),
                                ProjectAccess::Read,
                            )
                            .await
                            .is_ok();
                            readable.insert(project_key, ok);
                            ok
                        }
                    };
                    if can_read {
                        own.push(row.clone());
                    }
                }
                if own.is_empty() {
                    continue;
                }
                let Some(default_ws) = reader
                    .find_workspace(ai_memory_core::DEFAULT_WORKSPACE_NAME.to_owned())
                    .await?
                else {
                    continue;
                };
                targets.push(Target {
                    label: format!("private profile of {}", user.username),
                    share,
                    workspace_id: default_ws,
                    viewer: Some(user.id),
                    candidates: own,
                });
            }
            Ok(targets)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn converge_target(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    llm: Option<&Arc<dyn LlmProvider>>,
    config: &ProfilePassConfig,
    target: &Target,
    merge_budget: &mut usize,
    report: &mut ProfilePassReport,
) -> Result<(), String> {
    let existing_scope = ai_memory_store::lookup_profile_scope(
        reader,
        target.share,
        target.workspace_id,
        target.viewer,
    )
    .await
    .map_err(|e| e.to_string())?;
    let (existing_pages, ledger) = match existing_scope {
        Some(scope) => reader
            .profile_scope_pages(scope.workspace_id, scope.project_id)
            .await
            .map_err(|e| e.to_string())?,
        None => (Vec::new(), Vec::new()),
    };
    let rows: Vec<&ProfileCandidateRow> = target.candidates.iter().collect();
    let plan = converge(
        &rows,
        &existing_pages,
        &ledger,
        config.settings.min_projects,
        min_contributors(target.share, config.distinguishes_operators),
    );
    report.entries_unchanged += plan.unchanged;
    report
        .skipped_manual
        .extend(plan.skipped_manual.iter().cloned());
    if plan.writes.is_empty() {
        return Ok(());
    }
    // Only now, with something to write, does the scope get created.
    let scope = ai_memory_store::create_profile_scope(
        reader,
        writer,
        target.share,
        target.workspace_id,
        target.viewer,
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut written = Vec::new();
    for mut entry in plan.writes {
        let stored = existing_pages
            .iter()
            .find(|p| p.path == entry.path)
            .and_then(|p| StoredText::of(&p.path, &p.title, &p.body, &p.frontmatter));
        // `place` already kept a settled entry's text; nothing to restate.
        let settled = stored
            .as_ref()
            .is_some_and(|stored| stored.statement == entry.statement);
        if let Some(llm) = llm
            && *merge_budget > 0
            && !settled
        {
            *merge_budget -= 1;
            let current = stored.as_ref().map(|stored| stored.statement.as_str());
            match merge_entry(llm.as_ref(), wiki.sanitizer(), current, &entry).await {
                Ok(merged)
                    if !merged.changed
                        && stored.as_ref().is_some_and(|stored| {
                            !negation_flipped(&stored.statement, &entry.statement)
                        }) =>
                {
                    // The evidence corroborates the entry: it stands as written.
                    report.llm_calls += 1;
                    if let Some(stored) = stored {
                        entry.statement = stored.statement;
                        entry.reasoning = stored.reasoning;
                        entry.applies_to = stored.applies_to;
                    }
                }
                Ok(merged) if !merged.changed => {
                    // A reversal the model called unchanged: keep the newest
                    // ruling as harvested rather than the stored text.
                    report.llm_calls += 1;
                }
                Ok(merged) => {
                    report.llm_calls += 1;
                    let statement = clean_line(&merged.statement, STATEMENT_MAX_BYTES);
                    if !statement.is_empty() {
                        entry.statement = statement;
                    }
                    let reasoning = clean_line(&merged.reasoning, REASONING_MAX_BYTES);
                    entry.reasoning = (!reasoning.is_empty()).then_some(reasoning);
                    let tags = known_tags(&merged.applies_to);
                    if !tags.is_empty() {
                        entry.applies_to = tags;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %entry.path, "profile merge failed; keeping the newest statement");
                    report.llm_fallbacks += 1;
                }
            }
        }
        let (frontmatter, body) = render_entry(&entry);
        let Ok(path) = PagePath::new(entry.path.clone()) else {
            report
                .errors
                .push(format!("invalid profile path {}", entry.path));
            continue;
        };
        match wiki
            .write_page(WritePageRequest {
                workspace_id: scope.workspace_id,
                project_id: scope.project_id,
                path,
                frontmatter,
                body,
                tier: Tier::Semantic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
                evidence: Vec::new(),
            })
            .await
        {
            Ok(_) => {
                report.entries_written += 1;
                written.push(ProfileLedgerEntry {
                    path: entry.path.clone(),
                    topic_key: entry.topic.clone(),
                    written_at: jiff::Timestamp::now().as_microsecond(),
                });
            }
            Err(e) => report.errors.push(format!("{}: {e}", entry.path)),
        }
    }
    if !written.is_empty() {
        writer
            .record_profile_entries(scope.workspace_id, scope.project_id, written)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One entry as `profile review` shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReviewEntry {
    /// Wiki path.
    pub path: String,
    /// Category.
    pub category: String,
    /// One-line statement.
    pub statement: String,
    /// Distinct projects behind it (harvested entries only).
    pub projects: Option<u64>,
    /// Confidence (harvested entries only).
    pub confidence: Option<f64>,
    /// Oldest evidence date.
    pub first_seen: Option<String>,
    /// Newest evidence date.
    pub last_seen: Option<String>,
    /// `general` or `project`.
    pub generality: Option<String>,
    /// Written by the harvester and unchanged since (it may update it);
    /// `false` for an entry written or edited by hand (it never will).
    pub managed: bool,
    /// The evidence the harvester recorded, newest first.
    pub evidence: serde_json::Value,
}

/// What `profile review` shows for one profile.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ProfileReview {
    /// Effective share, or `None` when the profile is off.
    pub share: Option<&'static str>,
    /// Current entries, most recently seen first.
    pub entries: Vec<ReviewEntry>,
    /// Groups still below the admission bar.
    pub waiting: Vec<WaitingGroup>,
    /// Entries the harvester would update next pass.
    pub pending_updates: Vec<String>,
    /// Hand-edited entries the harvester leaves alone.
    pub hand_edited: Vec<String>,
    /// Removed entries that stay removed until said again.
    pub forgotten: Vec<String>,
}

/// The review of one profile: the workspace's when `share = "workspace"`,
/// `user`'s when it is per user, the global one otherwise. Writes nothing.
///
/// # Errors
/// Store reads.
pub async fn profile_review(
    reader: &ReaderPool,
    config: &ProfilePassConfig,
    workspace_id: Option<WorkspaceId>,
    user: Option<UserId>,
) -> Result<ProfileReview, ProfilePassError> {
    let mut review = ProfileReview::default();
    let Some(share) = config
        .settings
        .effective_share(config.distinguishes_operators)
    else {
        return Ok(review);
    };
    review.share = Some(share.as_str());
    let candidates = reader.profile_candidates(PROFILE_CANDIDATES_LIMIT).await?;
    let targets = convergence_targets(reader, share, config, &candidates).await?;
    let target = targets.into_iter().find(|t| match share {
        EffectiveProfileShare::Global => true,
        EffectiveProfileShare::Workspace => Some(t.workspace_id) == workspace_id,
        EffectiveProfileShare::User => t.viewer.is_some() && t.viewer == user,
    });
    let scope_workspace = match share {
        EffectiveProfileShare::Workspace => workspace_id,
        _ => target.as_ref().map(|t| t.workspace_id),
    };
    let scope = match scope_workspace {
        Some(ws) => ai_memory_store::lookup_profile_scope(reader, share, ws, user).await?,
        None if share == EffectiveProfileShare::Global => {
            ai_memory_store::lookup_global_scope(reader).await?
        }
        None => None,
    };
    let (pages, ledger) = match scope {
        Some(scope) => {
            reader
                .profile_scope_pages(scope.workspace_id, scope.project_id)
                .await?
        }
        None => (Vec::new(), Vec::new()),
    };
    let plan = review_plan(
        target.as_ref().map_or(&[][..], |t| &t.candidates[..]),
        &pages,
        &ledger,
        config.settings.min_projects,
        min_contributors(share, config.distinguishes_operators),
    );
    review.waiting = plan.waiting;
    review.pending_updates = plan.writes.into_iter().map(|w| w.path).collect();
    review.hand_edited = plan.skipped_manual;
    review.forgotten = plan.forgotten;
    let mut entries: Vec<ReviewEntry> = pages
        .iter()
        .filter_map(|page| {
            let entry =
                ProfileEntry::from_page(&page.path, &page.title, &page.body, &page.frontmatter)?;
            let fm = &page.frontmatter;
            let text = |key: &str| {
                fm.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            Some(ReviewEntry {
                category: entry.category().to_owned(),
                path: page.path.clone(),
                statement: entry.statement,
                projects: fm.get("projects").and_then(serde_json::Value::as_u64),
                confidence: fm.get("confidence").and_then(serde_json::Value::as_f64),
                first_seen: text("first_seen"),
                last_seen: text("last_seen"),
                generality: text("generality"),
                managed: ExistingEntry::from_page(page).managed,
                evidence: fm
                    .get("evidence")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.last_seen
            .cmp(&a.last_seen)
            .then_with(|| a.path.cmp(&b.path))
    });
    review.entries = entries;
    Ok(review)
}

/// The read-only view `profile review` shows for one scope: its candidates
/// converged against its pages, without writing anything.
#[must_use]
pub fn review_plan(
    candidates: &[ProfileCandidateRow],
    existing_pages: &[ProfileScopePage],
    ledger: &[ProfileLedgerEntry],
    min_projects: u32,
    min_contributors: usize,
) -> ConvergePlan {
    let rows: Vec<&ProfileCandidateRow> = candidates.iter().collect();
    converge(
        &rows,
        existing_pages,
        ledger,
        min_projects,
        min_contributors,
    )
}

/// Distinct operators a statement needs evidence from before it enters
/// `share`'s profile: two for a team profile, one otherwise.
#[must_use]
pub fn min_contributors(share: EffectiveProfileShare, distinguishes_operators: bool) -> usize {
    if ai_memory_core::profile::is_team_profile(share, distinguishes_operators) {
        TEAM_MIN_CONTRIBUTORS
    } else {
        1
    }
}

/// Distinct operators a team-profile statement needs evidence from.
pub const TEAM_MIN_CONTRIBUTORS: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::ProjectId;

    fn row(
        project: &str,
        statement: &str,
        observed_at: i64,
        generality: ProfileGenerality,
    ) -> ProfileCandidateRow {
        let ws = WorkspaceId::from_slice(&[1; 16]).unwrap();
        let mut proj = [0u8; 16];
        let digest = Sha256::digest(project.as_bytes());
        proj.copy_from_slice(&digest[..16]);
        ProfileCandidateRow {
            workspace_id: ws,
            project_id: ProjectId::from_slice(&proj).unwrap(),
            workspace: "default".to_owned(),
            project: project.to_owned(),
            restricted: false,
            candidate: NewProfileCandidate {
                source: ProfileCandidateSource::Prompt,
                source_ref: format!("session:{project}-{observed_at}"),
                topic_key: topic_key(statement),
                category: category_for(statement).to_owned(),
                statement: statement.to_owned(),
                quote: statement.to_owned(),
                applies_to: applies_to_for(statement),
                observed_at,
                contributor: None,
                generality,
                confidence: 0.5,
            },
        }
    }

    fn day(n: i64) -> i64 {
        1_790_000_000_000_000 + n * 86_400_000_000
    }

    /// A marker quoted inside a sentence is the user talking about it, not
    /// a pasted block: the whole prompt is harvested. A block whose markers
    /// sit on their own lines is still dropped (control).
    #[test]
    fn a_marker_quoted_mid_sentence_keeps_the_prompt() {
        let quoted = "Always use pnpm. The `<!-- ai-memory:start -->` marker sits in \
                      CLAUDE.md; never run npm";
        assert_eq!(user_written(quoted).as_deref(), Some(quoted));
        assert!(
            detect_preferences(&user_written(quoted).unwrap_or_default())
                .iter()
                .any(|found| found.statement.contains("never run npm"))
        );

        let pasted = "Always use pnpm.\n<!-- ai-memory:start -->\nNever write notes.\n\
                      <!-- ai-memory:end -->\nKeep commits small.";
        assert_eq!(
            user_written(pasted).as_deref(),
            Some("Always use pnpm.\n\nKeep commits small.")
        );
    }

    /// A page metadata field stored by an earlier harvest never waits for
    /// promotion; the same page's real statement still does (control).
    #[test]
    fn a_stored_page_metadata_field_is_not_a_waiting_habit() {
        let page = |project: &str, statement: &str| {
            let mut r = row(project, statement, day(1), ProfileGenerality::Project);
            r.candidate.source = ProfileCandidateSource::Page;
            r.candidate.source_ref = format!("page:decisions/{project}.md");
            r
        };
        let status = page("alpha", "Status:** Accepted");
        let date = page("beta", "Date:** 2026-10-01");
        let decision = page("alpha", "Every store opens SQLite in WAL mode.");
        let plan = converge(&[&status, &date, &decision], &[], &[], 2, 1);
        let waiting: Vec<&str> = plan.waiting.iter().map(|w| w.statement.as_str()).collect();
        assert_eq!(waiting, ["Every store opens SQLite in WAL mode."]);

        // The label decides, not the value's length: a long ADR template
        // field is dropped, a terse labelled rule waits.
        let template = page(
            "alpha",
            "Status:** accepted <!-- proposed | accepted | superseded by [[decisions/other]] -->",
        );
        let rule = page("beta", "Package manager:** pnpm");
        let plan = converge(&[&template, &rule], &[], &[], 2, 1);
        let waiting: Vec<&str> = plan.waiting.iter().map(|w| w.statement.as_str()).collect();
        assert_eq!(waiting, ["Package manager:** pnpm"]);

        // The same words from a prompt are not a page field and stay.
        let said = row(
            "alpha",
            "Status:** Accepted",
            day(1),
            ProfileGenerality::Project,
        );
        assert_eq!(converge(&[&said], &[], &[], 2, 1).waiting.len(), 1);
    }

    /// Agent output in the prompt channel (a lead agent's brief, a pasted
    /// review) is not the user stating a preference (#1148).
    #[test]
    fn agent_written_sentences_are_not_candidates() {
        for text in [
            "Always check src/router.rs:120 before you touch the gate.",
            "Never edit crates/store/src/ops.rs:600-640 by hand.",
            "**Never** run the migration twice.",
            "Always keep __init__ files empty.",
        ] {
            assert!(looks_agent_written(text), "{text}");
            assert!(detect_preferences(text).is_empty(), "{text}");
            assert!(!has_marker(text), "{text}");
        }
        for text in [
            "Always use pnpm for installs.",
            "I prefer a 3:2 split for the sidebar layout.",
            "Never commit generated files like build.rs output.",
        ] {
            assert!(!looks_agent_written(text), "{text}");
            assert!(!detect_preferences(text).is_empty(), "{text}");
        }
    }

    /// The same brief fanned out to two workers' checkouts within minutes is
    /// one message; the same words typed in two projects days apart is a
    /// habit (#1148).
    #[test]
    fn a_fanned_out_brief_counts_once_and_a_repeated_habit_twice() {
        let brief = "Always run the full suite before you reply.";
        let alpha = row("worker-a", brief, day(1), ProfileGenerality::Project);
        let beta = row(
            "worker-b",
            brief,
            day(1) + 60_000_000,
            ProfileGenerality::Project,
        );
        let plan = converge(&[&alpha, &beta], &[], &[], 2, 1);
        assert!(plan.writes.is_empty(), "{:?}", plan.writes);
        assert_eq!(plan.waiting.len(), 1);
        assert_eq!(plan.waiting[0].needs, 1);

        let later = row("worker-b", brief, day(3), ProfileGenerality::Project);
        let plan = converge(&[&alpha, &later], &[], &[], 2, 1);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].projects, 2);
    }

    /// One quote backs one entry even when it was classified into two
    /// topics: no `-2`/`-3` duplicates of the same sentence (#1148).
    #[test]
    fn one_quote_backs_one_entry() {
        let quote = "In all my projects, use pnpm and keep commits small.";
        let mut pnpm = row(
            "alpha",
            "Use pnpm for every project.",
            day(1),
            ProfileGenerality::General,
        );
        pnpm.candidate.quote = quote.to_owned();
        let mut commits = row(
            "alpha",
            "Keep commits small everywhere.",
            day(1),
            ProfileGenerality::General,
        );
        commits.candidate.quote = quote.to_owned();
        commits.candidate.source_ref = "session:alpha-other".to_owned();
        let plan = converge(&[&pnpm, &commits], &[], &[], 2, 1);
        assert_eq!(plan.writes.len(), 1, "{:?}", plan.writes);
    }

    #[test]
    fn detects_imperative_preferences_in_english_and_portuguese() {
        let found = detect_preferences(
            "Please always use pnpm in this repo. It always fails on CI though.\n\
             I prefer small commits over big ones.\nSempre rode os testes antes do commit.",
        );
        let statements: Vec<&str> = found.iter().map(|f| f.statement.as_str()).collect();
        assert_eq!(
            statements,
            [
                "Please always use pnpm in this repo.",
                "I prefer small commits over big ones.",
                "Sempre rode os testes antes do commit."
            ]
        );
        // A bare "always" / "sempre" is still a candidate, but not a
        // cross-project statement: it must earn `min_projects` (#1148).
        assert!(
            found
                .iter()
                .all(|f| f.generality == ProfileGenerality::Project)
        );
    }

    #[test]
    fn only_an_explicit_cross_project_scope_is_general() {
        for project_only in [
            "Always run the tests for foo.rs before pushing.",
            "Never use the staging database for this campaign.",
            "By default sign-up stays off in this app.",
            "From now on use the new endpoint here.",
            "Sempre rode os testes do módulo de pagamentos.",
            "Por padrão use o banco local neste app.",
        ] {
            assert_eq!(
                generality_of(project_only),
                ProfileGenerality::Project,
                "{project_only}"
            );
        }
        for general in [
            "In all my projects, write commit messages in English.",
            "Use pnpm in every project.",
            "Keep the same lint config across projects.",
            "By default everywhere, prefer small commits.",
            "Em todos os meus projetos, escreva commits em inglês.",
            "A partir de agora em todo projeto use pnpm.",
        ] {
            assert_eq!(
                generality_of(general),
                ProfileGenerality::General,
                "{general}"
            );
        }
    }

    #[test]
    fn questions_code_and_fragments_are_not_preferences() {
        assert!(detect_preferences("Should we always use pnpm?").is_empty());
        assert!(detect_preferences("```\nalways use npm here\n```").is_empty());
        assert!(detect_preferences("    always use npm here").is_empty());
        assert!(detect_preferences("never again").is_empty());
        assert!(detect_preferences("The build always fails on Fridays.").is_empty());
    }

    #[test]
    fn topic_overlap_groups_a_later_ruling_on_the_same_choice() {
        let a = topic_tokens("Always use pnpm.");
        let b = topic_tokens("Use bun instead of pnpm from now on.");
        let c = topic_tokens("Never commit secrets.");
        let d = topic_tokens("Run clippy before every commit.");
        assert!(topic_similarity(&a, &b) >= TOPIC_OVERLAP_THRESHOLD);
        assert!(topic_similarity(&c, &d) < TOPIC_OVERLAP_THRESHOLD);
    }

    #[test]
    fn categories_and_stack_scoping() {
        assert_eq!(category_for("Run pytest with -x"), "testing");
        assert_eq!(category_for("Squash commits before merge"), "workflow");
        assert_eq!(category_for("Use pnpm"), "tools");
        assert_eq!(category_for("Keep the API layer thin"), "architecture");
        assert_eq!(applies_to_for("Use pnpm and vitest"), ["javascript"]);
        assert_eq!(applies_to_for("Run cargo clippy"), ["rust"]);
        assert!(applies_to_for("Write small commits").is_empty());
    }

    #[test]
    fn a_choice_needs_min_projects_unless_stated_as_general() {
        let one = row(
            "alpha",
            "Use ruff for linting.",
            day(1),
            ProfileGenerality::Project,
        );
        let plan = converge(&[&one], &[], &[], 2, 1);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.waiting.len(), 1);
        assert_eq!(plan.waiting[0].needs, 1);

        let two = row(
            "beta",
            "Use ruff for linting!",
            day(2),
            ProfileGenerality::Project,
        );
        let plan = converge(&[&one, &two], &[], &[], 2, 1);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].projects, 2);

        let general = row(
            "gamma",
            "In all my projects, write docs in plain English.",
            day(3),
            ProfileGenerality::General,
        );
        let plan = converge(&[&general], &[], &[], 2, 1);
        assert_eq!(
            plan.writes.len(),
            1,
            "a general statement needs one project"
        );
    }

    #[test]
    fn the_latest_ruling_wins_within_a_group() {
        let old_a = row(
            "alpha",
            "Always use pnpm.",
            day(1),
            ProfileGenerality::General,
        );
        let old_b = row(
            "beta",
            "Always use pnpm.",
            day(2),
            ProfileGenerality::General,
        );
        let new_c = row(
            "gamma",
            "Use bun instead of pnpm from now on.",
            day(9),
            ProfileGenerality::General,
        );
        let plan = converge(&[&old_a, &old_b, &new_c], &[], &[], 2, 1);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(
            plan.writes[0].statement,
            "Use bun instead of pnpm from now on."
        );
        assert_eq!(plan.writes[0].projects, 3);
    }

    #[test]
    fn rendering_is_deterministic_and_unchanged_evidence_is_not_rewritten() {
        let a = row(
            "alpha",
            "Always use pnpm.",
            day(1),
            ProfileGenerality::General,
        );
        let plan = converge(&[&a], &[], &[], 2, 1);
        let (fm1, body1) = render_entry(&plan.writes[0]);
        let (fm2, body2) = render_entry(&plan.writes[0]);
        assert_eq!((fm1.clone(), body1.clone()), (fm2, body2));
        let page = ProfileScopePage {
            path: plan.writes[0].path.clone(),
            title: "x".into(),
            body: body1,
            frontmatter: fm1,
        };
        let again = converge(&[&a], &[page], &[], 2, 1);
        assert!(again.writes.is_empty());
        assert_eq!(again.unchanged, 1);
    }

    #[test]
    fn a_hand_edited_entry_is_never_rewritten() {
        let a = row(
            "alpha",
            "Always use pnpm.",
            day(1),
            ProfileGenerality::General,
        );
        let plan = converge(&[&a], &[], &[], 2, 1);
        let (fm, body) = render_entry(&plan.writes[0]);
        let edited = ProfileScopePage {
            path: plan.writes[0].path.clone(),
            title: "x".into(),
            body: format!("{body}\nMy own note."),
            frontmatter: fm,
        };
        let newer = row(
            "beta",
            "Always use pnpm, really.",
            day(5),
            ProfileGenerality::General,
        );
        let plan = converge(&[&a, &newer], std::slice::from_ref(&edited), &[], 2, 1);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.skipped_manual, std::slice::from_ref(&edited.path));

        let handwritten = ProfileScopePage {
            path: "profile/tools/pnpm.md".into(),
            title: "pnpm".into(),
            body: "Always use pnpm.".into(),
            frontmatter: serde_json::json!({}),
        };
        let plan = converge(&[&a], &[handwritten], &[], 2, 1);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.skipped_manual, ["profile/tools/pnpm.md"]);
    }

    #[test]
    fn a_forgotten_entry_stays_gone_until_the_user_says_it_again() {
        let a = row(
            "alpha",
            "Always use pnpm.",
            day(1),
            ProfileGenerality::General,
        );
        let path = converge(&[&a], &[], &[], 2, 1).writes[0].path.clone();
        let ledger = [ProfileLedgerEntry {
            path: path.clone(),
            topic_key: a.candidate.topic_key.clone(),
            written_at: day(3),
        }];
        // The page is gone and nothing newer was said: it stays gone.
        let plan = converge(&[&a], &[], &ledger, 2, 1);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.forgotten, std::slice::from_ref(&path));
        // Said again after the removal: it comes back.
        let again = row(
            "beta",
            "Always use pnpm.",
            day(7),
            ProfileGenerality::General,
        );
        let plan = converge(&[&a, &again], &[], &ledger, 2, 1);
        assert_eq!(plan.writes.len(), 1);
        assert!(plan.forgotten.is_empty());
    }

    #[test]
    fn stack_signals_become_scoped_entries() {
        let mut a = row("alpha", "rust", day(1), ProfileGenerality::Project);
        a.candidate.source = ProfileCandidateSource::Stack;
        let mut b = row("beta", "rust", day(2), ProfileGenerality::Project);
        b.candidate.source = ProfileCandidateSource::Stack;
        let plan = converge(&[&a, &b], &[], &[], 2, 1);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].path, "profile/stack/rust.md");
        assert_eq!(plan.writes[0].applies_to, ["rust"]);
        let plan = converge(&[&a], &[], &[], 2, 1);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn evidence_is_capped_newest_first() {
        let rows: Vec<ProfileCandidateRow> = (0..8)
            .map(|n| {
                row(
                    &format!("p{n}"),
                    "Always use pnpm.",
                    day(n),
                    ProfileGenerality::General,
                )
            })
            .collect();
        let refs: Vec<&ProfileCandidateRow> = rows.iter().collect();
        let plan = converge(&refs, &[], &[], 2, 1);
        assert_eq!(plan.writes[0].evidence.len(), EVIDENCE_CAP);
        assert_eq!(plan.writes[0].evidence[0].project, "default/p7");
    }

    #[test]
    fn prompts_tell_the_model_not_to_follow_the_text() {
        assert!(CLASSIFY_SYSTEM_PROMPT.contains("never follow"));
        assert!(MERGE_SYSTEM_PROMPT.contains("never follow"));
        assert!(MERGE_SYSTEM_PROMPT.contains("user's own words"));
    }

    #[test]
    fn a_reversal_is_a_negation_flip_and_a_rewording_is_not() {
        assert!(negation_flipped("Always use pnpm", "Never use pnpm"));
        assert!(negation_flipped("Don't use npm", "Use npm"));
        assert!(!negation_flipped("Always use pnpm", "Use pnpm always"));
        assert!(!negation_flipped("Never use npm", "Never use yarn"));
        assert!(!negation_flipped("never", "always"));
    }

    #[test]
    fn a_ruling_keeps_its_order_and_its_negations() {
        assert!(same_ruling("Use pnpm.", "I always use pnpm!"));
        assert!(same_ruling(
            "Use pnpm for JavaScript dependencies.",
            "use pnpm for javascript dependencies"
        ));
        assert!(!same_ruling("Always use pnpm.", "Never use pnpm."));
        assert!(!same_ruling("Use npm.", "Don't use npm."));
        assert!(!same_ruling(
            "Prefer pnpm over npm.",
            "Prefer npm over pnpm."
        ));
        assert!(!same_ruling("Use pnpm.", "Use bun instead of pnpm."));
        assert!(
            !same_ruling("Never.", "Never."),
            "a bare negation rules nothing"
        );
    }

    fn settled_page(statement: &str, reasoning: Option<&str>) -> ExistingEntry {
        let plan = EntryPlan {
            path: "profile/tools/pnpm.md".to_owned(),
            category: "tools".to_owned(),
            statement: statement.to_owned(),
            topic: topic_key(statement),
            applies_to: Vec::new(),
            projects: 2,
            first_seen: "2026-01-01".to_owned(),
            last_seen: "2026-01-02".to_owned(),
            confidence: 0.6,
            generality: ProfileGenerality::Project,
            evidence: Vec::new(),
            reasoning: reasoning.map(str::to_owned),
            is_new: false,
        };
        let (frontmatter, body) = render_entry(&plan);
        ExistingEntry {
            path: plan.path,
            tokens: topic_tokens(statement),
            managed: true,
            frontmatter,
            body,
        }
    }

    fn incoming(statement: &str) -> EntryPlan {
        EntryPlan {
            path: "profile/tools/pnpm.md".to_owned(),
            category: "tools".to_owned(),
            statement: statement.to_owned(),
            topic: topic_key(statement),
            applies_to: Vec::new(),
            projects: 3,
            first_seen: "2026-01-01".to_owned(),
            last_seen: "2026-01-09".to_owned(),
            confidence: 0.7,
            generality: ProfileGenerality::Project,
            evidence: Vec::new(),
            reasoning: None,
            is_new: false,
        }
    }

    /// Corroborating evidence keeps the stored statement and reasoning; a
    /// reversed ruling or a narrower scope still replaces them.
    #[test]
    fn corroboration_keeps_the_settled_text() {
        let page = settled_page("Use pnpm.", Some("Faster installs."));
        let mut same = incoming("I always use pnpm!");
        keep_settled_text(&page, &mut same);
        assert_eq!(same.statement, "Use pnpm.");
        assert_eq!(same.reasoning.as_deref(), Some("Faster installs."));

        let mut reversed = incoming("Never use pnpm.");
        keep_settled_text(&page, &mut reversed);
        assert_eq!(reversed.statement, "Never use pnpm.");

        let mut narrower = incoming("I always use pnpm!");
        narrower.applies_to = vec!["javascript".to_owned()];
        keep_settled_text(&page, &mut narrower);
        assert_eq!(narrower.statement, "I always use pnpm!");
    }

    /// Grouping compares a candidate only with groups sharing a token, once
    /// per distinct token set: 4,000 candidates on 400 topics take a few
    /// thousand comparisons, where the per-member scan took millions.
    #[test]
    fn grouping_compares_with_groups_not_every_member() {
        let rows: Vec<ProfileCandidateRow> = (0..4_000)
            .map(|i| {
                let topic = i % 400;
                row(
                    &format!("p{}", i % 7),
                    &format!("use tool{topic} for build{topic}"),
                    day(i64::from(i)),
                    ProfileGenerality::Project,
                )
            })
            .collect();
        let refs: Vec<&ProfileCandidateRow> = rows.iter().collect();
        let (groups, comparisons) = group_by_topic_counted(&refs);
        assert_eq!(groups.len(), 400);
        assert!(groups.iter().all(|g| g.len() == 10));
        assert!(comparisons <= 4_000, "comparisons: {comparisons}");
    }
}
