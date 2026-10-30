//! The compact intermediate contract of chunked processing: the digest a map
//! or reduce call returns, its server-side validation, and its rendering.
//!
//! A digest is not a proposal and not a page. It carries candidate facts with
//! references to observations the server can check, short exact quotes the
//! server can check, and explicit conflicts. Anything the server cannot tie to
//! the evidence it showed is dropped before a checkpoint is written.

use std::collections::{BTreeMap, BTreeSet};

use ai_memory_core::Sanitizer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::chunk_plan::{Span, text_cost};
use crate::auto_improve::{
    AutoImproveLlmResponse, AutoImproveProposal, AutoImproveRejectedCandidate,
};

pub(super) const MAP_SYSTEM_PROMPT: &str = include_str!("../../prompts/chunk_map_system.md");
pub(super) const REDUCE_SYSTEM_PROMPT: &str = include_str!("../../prompts/chunk_reduce_system.md");

/// Most items a digest may carry.
pub(super) const MAX_DIGEST_ITEMS: usize = 12;
/// Most conflicts a digest may carry.
pub(super) const MAX_DIGEST_CONFLICTS: usize = 4;
const MAX_TEXT_CHARS: usize = 240;
const MAX_QUOTE_CHARS: usize = 160;
const MAX_NOTE_CHARS: usize = 160;
const MAX_TOPIC_CHARS: usize = 80;
const MAX_REFS: usize = 6;
const MAX_SUPERSEDES: usize = 4;

/// What an item states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DigestKind {
    /// Something the evidence establishes.
    Fact,
    /// A choice that was made.
    Decision,
    /// A failure mode or surprise.
    Gotcha,
    /// An always/never instruction.
    Rule,
    /// A repeatable multi-step workflow.
    Procedure,
    /// Unresolved question or pending work.
    Open,
    /// Later evidence overriding an earlier claim.
    Correction,
}

impl DigestKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Gotcha => "gotcha",
            Self::Rule => "rule",
            Self::Procedure => "procedure",
            Self::Open => "open",
            Self::Correction => "correction",
        }
    }
}

/// One candidate fact with its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DigestItem {
    /// What the item states.
    pub kind: DigestKind,
    /// One self-contained sentence.
    pub text: String,
    /// Observation labels supporting it (`o12` or `o12@3000`).
    pub refs: Vec<String>,
    /// Short exact quote from the evidence, or empty.
    #[serde(default)]
    pub quote: String,
    /// Labels of earlier claims this item overrides.
    #[serde(default)]
    pub supersedes: Vec<String>,
}

/// An unresolved disagreement, kept explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DigestConflict {
    /// What the disagreement is about.
    pub topic: String,
    /// Labels of the observations on either side.
    pub refs: Vec<String>,
    /// Short neutral note.
    #[serde(default)]
    pub note: String,
}

/// Output of one map or reduce call. Both lists are required: an object that
/// names neither (`{"facts": [...]}`) must fail, not read as "nothing found".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkDigest {
    /// Candidate facts, at most 12.
    pub items: Vec<DigestItem>,
    /// Unresolved conflicts, at most 4.
    pub conflicts: Vec<DigestConflict>,
}

/// Closed wire envelope of a chunked auto-improve final call.
///
/// The legacy [`AutoImproveLlmResponse`] defaults every field and ignores
/// unknown ones, so `{}` or `{"facts": [...]}` would read as a completed
/// review with nothing found. A chunked job has already mapped candidates, so
/// the answer must name all three lists (each may be explicitly empty) and
/// nothing else. Items keep the legacy tolerant [`AutoImproveProposal`] shape.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewEnvelope {
    /// Short summary of the review.
    summary: String,
    /// Candidate edits.
    proposals: Vec<AutoImproveProposal>,
    /// Candidates the model chose not to promote.
    rejected_candidates: Vec<AutoImproveRejectedCandidate>,
}

impl From<ReviewEnvelope> for AutoImproveLlmResponse {
    fn from(e: ReviewEnvelope) -> Self {
        Self {
            summary: e.summary,
            proposals: e.proposals,
            rejected_candidates: e.rejected_candidates,
        }
    }
}

/// Cost of the JSON schema of `T` as sent to the provider.
pub(super) fn schema_cost<T: JsonSchema>() -> usize {
    serde_json::to_string(&schemars::schema_for!(T)).map_or(32_000, |s| text_cost(&s))
}

/// Which reference strings a call may legitimately cite.
pub(super) enum RefScope<'a> {
    /// Map: the primary spans shown in the chunk.
    Spans(&'a [Span]),
    /// Reduce: exactly the refs present in the child digests.
    Known(&'a BTreeSet<String>),
}

impl RefScope<'_> {
    fn canonical(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        match self {
            Self::Known(known) => known.contains(raw).then(|| raw.to_string()),
            Self::Spans(spans) => {
                let rest = raw.strip_prefix('o')?;
                let (ord, at) = match rest.split_once('@') {
                    Some((ord, at)) => (ord, Some(at.parse::<u64>().ok()?)),
                    None => (rest, None),
                };
                let ordinal: u32 = ord.parse().ok()?;
                let mut of_ordinal = spans.iter().filter(|s| s.ordinal == ordinal).peekable();
                let first = of_ordinal.peek().copied()?;
                match at {
                    None => Some(first.label()),
                    Some(at) => spans
                        .iter()
                        .filter(|s| s.ordinal == ordinal)
                        .find(|s| s.start <= at && (at < s.end || at == s.start))
                        .map(Span::label),
                }
            }
        }
    }
}

/// How a quote is verified.
pub(super) enum QuoteCheck<'a> {
    /// Map: the quote must occur in the rendered evidence (whitespace-folded).
    Evidence(&'a str),
    /// Reduce: the quote must be one a child already carried.
    Known(&'a BTreeSet<String>),
}

/// Why a digest was refused (always recoverable by splitting the input).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DigestRejection {
    /// More items or conflicts than the contract allows.
    TooMany,
    /// Still above the per-node cost cap after dropping quotes.
    Oversize,
}

/// A validated digest and how many unsupported items were dropped.
#[derive(Debug)]
pub(super) struct Validated {
    pub digest: ChunkDigest,
    pub dropped: u32,
}

fn fold_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn flat_clip(sanitizer: &Sanitizer, s: &str, max: usize) -> String {
    let scrubbed = sanitizer.scrub(s);
    fold_ws(&scrubbed).chars().take(max).collect()
}

fn first_ordinal(refs: &[String]) -> u32 {
    refs.iter()
        .filter_map(|r| r.strip_prefix('o'))
        .filter_map(|r| r.split('@').next())
        .filter_map(|r| r.parse::<u32>().ok())
        .min()
        .unwrap_or(u32::MAX)
}

/// Check a raw model digest against what the server showed it.
///
/// # Errors
/// [`DigestRejection`] when the digest cannot be accepted as one node.
pub(super) fn validate_digest(
    raw: ChunkDigest,
    scope: &RefScope<'_>,
    quotes: &QuoteCheck<'_>,
    sanitizer: &Sanitizer,
    cost_cap: usize,
) -> Result<Validated, DigestRejection> {
    if raw.items.len() > MAX_DIGEST_ITEMS || raw.conflicts.len() > MAX_DIGEST_CONFLICTS {
        return Err(DigestRejection::TooMany);
    }
    let folded_evidence = match quotes {
        QuoteCheck::Evidence(text) => Some(fold_ws(text)),
        QuoteCheck::Known(_) => None,
    };
    let mut dropped = 0u32;
    let mut items: Vec<DigestItem> = Vec::new();
    for item in raw.items {
        let text = flat_clip(sanitizer, &item.text, MAX_TEXT_CHARS);
        let mut refs: Vec<String> = Vec::new();
        for r in &item.refs {
            if let Some(c) = scope.canonical(r)
                && !refs.contains(&c)
                && refs.len() < MAX_REFS
            {
                refs.push(c);
            }
        }
        if text.is_empty() || refs.is_empty() {
            dropped += 1;
            continue;
        }
        let quote = flat_clip(sanitizer, &item.quote, MAX_QUOTE_CHARS);
        let quote_ok = quote.is_empty()
            || match quotes {
                QuoteCheck::Evidence(_) => folded_evidence
                    .as_deref()
                    .is_some_and(|ev| ev.contains(&quote)),
                QuoteCheck::Known(known) => known.contains(&quote),
            };
        let mut supersedes: Vec<String> = Vec::new();
        for r in &item.supersedes {
            if let Some(c) = scope.canonical(r)
                && !supersedes.contains(&c)
                && supersedes.len() < MAX_SUPERSEDES
            {
                supersedes.push(c);
            }
        }
        let candidate = DigestItem {
            kind: item.kind,
            text,
            refs,
            quote: if quote_ok { quote } else { String::new() },
            supersedes,
        };
        if let Some(existing) = items
            .iter_mut()
            .find(|e| e.kind == candidate.kind && e.text == candidate.text)
        {
            for r in candidate.refs {
                if !existing.refs.contains(&r) && existing.refs.len() < MAX_REFS {
                    existing.refs.push(r);
                }
            }
            // A later restatement may carry the correction that supersedes
            // older evidence, or the quote the first one lacked: keep both.
            for s in candidate.supersedes {
                if !existing.supersedes.contains(&s) && existing.supersedes.len() < MAX_SUPERSEDES {
                    existing.supersedes.push(s);
                }
            }
            if existing.quote.is_empty() {
                existing.quote = candidate.quote;
            }
        } else {
            items.push(candidate);
        }
    }
    let mut conflicts: Vec<DigestConflict> = Vec::new();
    for c in raw.conflicts {
        let topic = flat_clip(sanitizer, &c.topic, MAX_TOPIC_CHARS);
        let mut refs: Vec<String> = Vec::new();
        for r in &c.refs {
            if let Some(canon) = scope.canonical(r)
                && !refs.contains(&canon)
                && refs.len() < MAX_REFS
            {
                refs.push(canon);
            }
        }
        if topic.is_empty() || refs.is_empty() {
            dropped += 1;
            continue;
        }
        conflicts.push(DigestConflict {
            topic,
            refs,
            note: flat_clip(sanitizer, &c.note, MAX_NOTE_CHARS),
        });
    }
    // Stable chronological order by earliest supporting observation.
    items.sort_by_key(|i| first_ordinal(&i.refs));
    conflicts.sort_by_key(|c| first_ordinal(&c.refs));
    let mut digest = ChunkDigest { items, conflicts };
    if digest_cost(&digest) > cost_cap {
        for item in &mut digest.items {
            item.quote.clear();
        }
        if digest_cost(&digest) > cost_cap {
            return Err(DigestRejection::Oversize);
        }
    }
    Ok(Validated { digest, dropped })
}

fn item_line(item: &DigestItem) -> String {
    let mut line = format!(
        "- {} | {} | refs={}",
        item.kind.as_str(),
        item.text,
        item.refs.join(",")
    );
    if !item.quote.is_empty() {
        line.push_str(&format!(" | quote=\"{}\"", item.quote));
    }
    if !item.supersedes.is_empty() {
        line.push_str(&format!(" | supersedes={}", item.supersedes.join(",")));
    }
    line
}

/// The digest as prompt lines.
#[must_use]
pub(super) fn render_digest(digest: &ChunkDigest) -> String {
    let mut out = String::new();
    for item in &digest.items {
        out.push_str(&item_line(item));
        out.push('\n');
    }
    for c in &digest.conflicts {
        out.push_str(&format!(
            "! {} | refs={} | {}\n",
            c.topic,
            c.refs.join(","),
            c.note
        ));
    }
    out
}

/// Cost of the rendered digest.
#[must_use]
pub(super) fn digest_cost(digest: &ChunkDigest) -> usize {
    text_cost(&render_digest(digest))
}

/// Every ref and quote a digest carries (the scope its parent may cite).
pub(super) fn collect_refs_and_quotes(
    digest: &ChunkDigest,
    refs: &mut BTreeSet<String>,
    quotes: &mut BTreeSet<String>,
) {
    for item in &digest.items {
        refs.extend(item.refs.iter().cloned());
        refs.extend(item.supersedes.iter().cloned());
        if !item.quote.is_empty() {
            quotes.insert(item.quote.clone());
        }
    }
    for c in &digest.conflicts {
        refs.extend(c.refs.iter().cloned());
    }
}

/// Replace every string in `value` by its scrubbed form.
pub(super) fn scrub_value(value: &mut serde_json::Value, sanitizer: &Sanitizer) {
    match value {
        serde_json::Value::String(s) => *s = sanitizer.scrub(s),
        serde_json::Value::Array(items) => {
            for v in items {
                scrub_value(v, sanitizer);
            }
        }
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            let mut scrubbed = BTreeMap::new();
            for k in keys {
                if let Some(mut v) = map.remove(&k) {
                    scrub_value(&mut v, sanitizer);
                    scrubbed.insert(k, v);
                }
            }
            map.extend(scrubbed);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans() -> Vec<Span> {
        vec![
            Span {
                ordinal: 3,
                start: 0,
                end: 10,
                total: 10,
            },
            Span {
                ordinal: 7,
                start: 100,
                end: 200,
                total: 300,
            },
        ]
    }

    fn item(kind: DigestKind, text: &str, refs: &[&str], quote: &str) -> DigestItem {
        DigestItem {
            kind,
            text: text.into(),
            refs: refs.iter().map(ToString::to_string).collect(),
            quote: quote.into(),
            supersedes: Vec::new(),
        }
    }

    fn run(items: Vec<DigestItem>, evidence: &str) -> Result<Validated, DigestRejection> {
        let sp = spans();
        validate_digest(
            ChunkDigest {
                items,
                conflicts: Vec::new(),
            },
            &RefScope::Spans(&sp),
            &QuoteCheck::Evidence(evidence),
            &Sanitizer::builtin(),
            4_000,
        )
    }

    #[test]
    fn unsupported_items_and_foreign_refs_are_dropped_and_counted() {
        let v = run(
            vec![
                item(DigestKind::Fact, "grounded", &["o3", "o99", "bogus"], ""),
                item(DigestKind::Fact, "no valid ref", &["o99"], ""),
                item(DigestKind::Decision, "fragment", &["o7@150"], ""),
            ],
            "irrelevant",
        )
        .unwrap();
        assert_eq!(v.dropped, 1);
        assert_eq!(v.digest.items.len(), 2);
        assert_eq!(v.digest.items[0].refs, vec!["o3".to_string()]);
        // an offset inside a fragment canonicalises to the fragment's label
        assert_eq!(v.digest.items[1].refs, vec!["o7@100".to_string()]);
    }

    #[test]
    fn a_quote_must_be_in_the_evidence() {
        let v = run(
            vec![
                item(DigestKind::Gotcha, "real", &["o3"], "error:   disk  full"),
                item(DigestKind::Gotcha, "fake quote", &["o7"], "never said this"),
            ],
            "the build said error: disk full today",
        )
        .unwrap();
        assert_eq!(v.digest.items.len(), 2);
        assert_eq!(v.digest.items[0].quote, "error: disk full");
        assert_eq!(v.digest.items[1].quote, "");
    }

    #[test]
    fn a_restated_item_keeps_the_correction_and_the_quote_of_either_copy() {
        let mut first = item(DigestKind::Decision, "use sqlite", &["o3"], "");
        let mut second = item(DigestKind::Decision, "use sqlite", &["o7"], "kept quote");
        first.supersedes = vec!["o3".into()];
        second.supersedes = vec!["o7".into(), "o3".into()];
        let v = run(vec![first, second], "a kept quote here").unwrap();
        assert_eq!(v.digest.items.len(), 1);
        let merged = &v.digest.items[0];
        assert_eq!(merged.quote, "kept quote");
        assert_eq!(
            merged.supersedes,
            vec!["o3".to_string(), "o7@100".to_string()]
        );
        assert_eq!(merged.refs, vec!["o3".to_string(), "o7@100".to_string()]);
    }

    #[test]
    fn too_many_items_and_oversize_are_refused_not_truncated() {
        let many: Vec<DigestItem> = (0..13)
            .map(|i| item(DigestKind::Fact, &format!("f{i}"), &["o3"], ""))
            .collect();
        assert_eq!(run(many, "").unwrap_err(), DigestRejection::TooMany);
        let sp = spans();
        let distinct: Vec<DigestItem> = (0..12)
            .map(|i| {
                item(
                    DigestKind::Fact,
                    &format!("{i} {}", "w ".repeat(100)),
                    &["o3"],
                    "",
                )
            })
            .collect();
        let r = validate_digest(
            ChunkDigest {
                items: distinct,
                conflicts: Vec::new(),
            },
            &RefScope::Spans(&sp),
            &QuoteCheck::Evidence(""),
            &Sanitizer::builtin(),
            500,
        );
        assert_eq!(r.unwrap_err(), DigestRejection::Oversize);
    }

    #[test]
    fn reduce_scope_accepts_only_child_refs_and_quotes() {
        let known: BTreeSet<String> = ["o1".into(), "o9@40".into()].into_iter().collect();
        let quotes: BTreeSet<String> = ["kept".into()].into_iter().collect();
        let v = validate_digest(
            ChunkDigest {
                items: vec![
                    item(DigestKind::Rule, "merged", &["o1", "o2"], "kept"),
                    item(DigestKind::Rule, "invented", &["o2"], ""),
                    item(DigestKind::Fact, "other", &["o9@40"], "made up"),
                ],
                conflicts: vec![DigestConflict {
                    topic: "port".into(),
                    refs: vec!["o1".into(), "o9@40".into()],
                    note: "a vs b".into(),
                }],
            },
            &RefScope::Known(&known),
            &QuoteCheck::Known(&quotes),
            &Sanitizer::builtin(),
            4_000,
        )
        .unwrap();
        assert_eq!(v.digest.items.len(), 2);
        assert_eq!(v.digest.items[0].refs, vec!["o1".to_string()]);
        assert_eq!(v.digest.items[1].quote, "");
        assert_eq!(v.digest.conflicts.len(), 1);
        assert_eq!(v.dropped, 1);
    }

    fn parse(v: serde_json::Value) -> Result<ChunkDigest, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn envelopes_with_wrong_missing_or_unknown_fields_are_rejected() {
        let ok_item = serde_json::json!({"kind": "fact", "text": "t", "refs": ["o1"]});
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"facts": [ok_item.clone()]}),
            serde_json::json!({"items": [ok_item.clone()]}),
            serde_json::json!({"conflicts": []}),
            serde_json::json!({"items": [], "conflicts": [], "summary": "x"}),
            serde_json::json!({"items": [{"kind": "fact", "text": "t", "refs": ["o1"], "ref": "o1"}], "conflicts": []}),
            serde_json::json!({"items": [{"kind": "fact", "text": "t"}], "conflicts": []}),
            serde_json::json!({"items": [{"kind": "note", "text": "t", "refs": ["o1"]}], "conflicts": []}),
            serde_json::json!({"items": [], "conflicts": [{"topic": "a", "refs": ["o1"], "reason": "x"}]}),
        ] {
            assert!(parse(bad.clone()).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn explicit_empty_and_full_envelopes_are_accepted() {
        let empty = parse(serde_json::json!({"items": [], "conflicts": []})).unwrap();
        assert_eq!(empty, ChunkDigest::default());
        let full = parse(serde_json::json!({
            "items": [{"kind": "correction", "text": "t", "refs": ["o1@3000"],
                       "quote": "q", "supersedes": ["o1"]}],
            "conflicts": [{"topic": "port", "refs": ["o1", "o2"], "note": "a vs b"}]
        }))
        .unwrap();
        assert_eq!(full.items[0].kind, DigestKind::Correction);
        assert_eq!(full.conflicts[0].topic, "port");
    }

    #[test]
    fn the_advertised_schema_requires_both_lists_and_closes_objects() {
        let schema = serde_json::to_value(schemars::schema_for!(ChunkDigest)).unwrap();
        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["conflicts", "items"]);
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    fn review(v: serde_json::Value) -> Result<AutoImproveLlmResponse, serde_json::Error> {
        serde_json::from_value::<ReviewEnvelope>(v).map(Into::into)
    }

    #[test]
    fn a_final_review_must_name_all_three_lists_and_nothing_else() {
        let proposal = serde_json::json!({"path": "gotchas/a.md", "title": "A"});
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"facts": [{"text": "x"}]}),
            serde_json::json!({"summary": "s", "proposals": []}),
            serde_json::json!({"summary": "s", "rejected_candidates": []}),
            serde_json::json!({"proposals": [proposal.clone()], "rejected_candidates": []}),
            serde_json::json!({"summary": "s", "proposals": [], "rejected_candidates": [],
                               "items": []}),
            serde_json::json!({"summary": "s", "proposals": "none", "rejected_candidates": []}),
        ] {
            assert!(review(bad.clone()).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn an_explicitly_empty_or_populated_final_review_is_accepted() {
        let empty = review(serde_json::json!({
            "summary": "", "proposals": [], "rejected_candidates": []
        }))
        .unwrap();
        assert!(empty.proposals.is_empty() && empty.rejected_candidates.is_empty());
        let full = review(serde_json::json!({
            "summary": "one lesson",
            "proposals": [{"path": "gotchas/a.md", "title": "A", "kind": "gotcha",
                           "confidence": 0.9, "body_markdown": "# A"}],
            "rejected_candidates": [{"reason": "transient", "evidence": "o3"}]
        }))
        .unwrap();
        assert_eq!(full.summary, "one lesson");
        assert_eq!(full.proposals[0].path, "gotchas/a.md");
        assert_eq!(full.rejected_candidates[0].reason, "transient");
    }
}
