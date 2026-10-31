//! Rule-based session-page synthesis (no LLM).
//!
//! At `SessionEnd` we turn a session's observations into a single markdown
//! page under `wiki/sessions/<id>.md` using only deterministic heuristics:
//! first-prompt as title, files touched, tool-call counts. Once the LLM
//! provider lands in M6 we'll add an opt-in path that re-narrates the page.
//!
//! The page is rendered from a [`SessionDigest`], not from the observation
//! rows. A session can hold hundreds of thousands of rows, and everything the
//! page and the auto-handoff print is either a count, a "last of a kind"
//! marker, the head and tail the raw-observation list shows, or the first or
//! last row that passes a predicate. [`SessionDigest::load`] reads exactly
//! those with bounded queries, so the memory a SessionEnd spends does not grow
//! with the session, and the output is the one the full row list produced —
//! the one exception being the `## Prompts` list, which is capped like the
//! raw-observation list.

use std::collections::{BTreeMap, BTreeSet};

use ai_memory_core::{
    AgentKind, FAILED_TOOL_OUTCOME_LINE, NewPage, Observation, ObservationKind, PagePath,
    ProjectId, SessionId, TOOL_BODY_HEADER_MAX_CHARS, TOOL_BODY_SEPARATOR, Tier, WorkspaceId,
    looks_like_markup_block, looks_like_scaffolding, truncate_for_title, truncate_utf8_bytes,
};
use ai_memory_store::{
    ObservationOrder, ReaderPool, SessionKindSummary, SessionLine, SessionObservationSnapshot,
    StoreError, StoreResult,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::capture_policy::ToolFamily;
use crate::payload::{
    is_safe_tool_title, listable_tool_name, tool_family_from_title, tool_title_display,
};

const RAW_OBSERVATION_MAX_LINES: usize = 500;
const RAW_OBSERVATION_HEAD_LINES: usize = 250;
const RAW_OBSERVATION_TAIL_LINES: usize = RAW_OBSERVATION_MAX_LINES - RAW_OBSERVATION_HEAD_LINES;
/// The `## Prompts` list is capped the way the raw-observation list is: a
/// session with more prompts than this prints its first and last slice and
/// an omission line between them. The values are the same bound, so the page
/// never holds more prompt titles than raw-observation lines.
pub(crate) const PROMPT_TITLES_MAX: usize = RAW_OBSERVATION_MAX_LINES;
pub(crate) const PROMPT_TITLES_HEAD: usize = RAW_OBSERVATION_HEAD_LINES;
pub(crate) const PROMPT_TITLES_TAIL: usize = PROMPT_TITLES_MAX - PROMPT_TITLES_HEAD;
const SUBAGENT_PROMPT_PREAMBLE: &str = "You are a subagent spawned by another session.";

/// Build a [`NewPage`] from the digest of a session's observations.
///
/// The returned page is *always* under `sessions/<session-id>.md` so each
/// session has a stable URL the user can bookmark.
#[must_use]
pub fn synthesize_session_page(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    agent_kind: AgentKind,
    digest: &SessionDigest,
) -> NewPage {
    // The body renderer and the summary builder describe the same session and
    // read the same digest, so they cannot describe it differently.
    let title = digest.title.clone();
    let body = render_body(session_id, digest, &title);
    let mut frontmatter_json = serde_json::json!({
        "title": title,
        "session_id": session_id.to_string(),
        // Origin, not writer: callers pass the immutable value persisted on
        // the session row, so checkpoints and later superseding versions keep
        // naming the harness that produced the session.
        "agent": agent_kind.as_str(),
        "tier": "episodic",
    });
    let summary = session_summary(digest);
    if !summary.is_empty() {
        // Insert only when there is something to say. The store's hit
        // descriptor prefers `summary` over the body, so writing a blank one
        // would trade real body text for nothing.
        frontmatter_json["summary"] = serde_json::Value::String(summary);
    }
    let path = PagePath::new(format!("sessions/{session_id}.md"))
        .expect("hard-coded sessions/<uuid>.md is always valid");
    NewPage {
        workspace_id,
        project_id,
        path,
        title: title.clone(),
        body,
        tier: Tier::Episodic,
        frontmatter_json,
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

/// A user prompt's text for the handoff and the abrupt-ending check: the body
/// when there is one, the title otherwise.
fn prompt_text(obs: &Observation) -> &str {
    if obs.body.is_empty() {
        obs.title.as_str()
    } else {
        obs.body.as_str()
    }
}

/// One user prompt, as far as the open-question heuristics read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptFact {
    pub(crate) title: String,
    pub(crate) body: String,
}

/// The terminal `SessionEnd` observation, as far as the page reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionEndFact {
    created_at: Timestamp,
    body: String,
}

/// Bytes of error text the page and the handoff quote.
const FAILED_TOOL_EXCERPT_BYTES: usize = 240;

/// A tool call the harness reported as failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedToolFact {
    /// A recorded tool name or a family label.
    pub(crate) title: String,
    /// Error text after the header, whitespace-collapsed and capped at
    /// [`FAILED_TOOL_EXCERPT_BYTES`]; empty when none was stored.
    pub(crate) error: String,
}

impl FailedToolFact {
    /// `"Bash: Exit code 1 …"`, or the error alone when the title is a family
    /// label, which names no tool.
    pub(crate) fn describe(&self) -> String {
        let error = if self.error.is_empty() {
            "(no error text captured)"
        } else {
            self.error.as_str()
        };
        match listable_tool_name(&self.title) {
            Some(name) => format!("{name}: {error}"),
            None => error.to_string(),
        }
    }
}

/// A `post-tool-use` whose header (the text before the first
/// [`TOOL_BODY_SEPARATOR`], within the first [`TOOL_BODY_HEADER_MAX_CHARS`]
/// characters) ends in [`FAILED_TOOL_OUTCOME_LINE`]. The store counts with
/// the same rule, so an `outcome: error` line inside a tool's output never
/// counts.
fn is_failed_tool(obs: &Observation) -> bool {
    if obs.kind != ObservationKind::PostToolUse {
        return false;
    }
    let body = obs.body.as_str();
    let head = body
        .char_indices()
        .nth(TOOL_BODY_HEADER_MAX_CHARS)
        .map_or(body, |(end, _)| &body[..end]);
    let header = head
        .split_once(TOOL_BODY_SEPARATOR)
        .map_or(head, |(header, _)| header);
    header.ends_with(FAILED_TOOL_OUTCOME_LINE)
}

/// [`is_failed_tool`] with the quoted error text.
fn failed_tool_fact(obs: &Observation) -> Option<FailedToolFact> {
    if !is_failed_tool(obs) {
        return None;
    }
    let output = obs
        .body
        .split_once(TOOL_BODY_SEPARATOR)
        .map_or("", |(_, output)| output);
    let collapsed = output.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(FailedToolFact {
        title: obs.title.clone(),
        error: truncate_utf8_bytes(&collapsed, FAILED_TOOL_EXCERPT_BYTES),
    })
}

/// Everything the auto-handoff and the abrupt-ending note read from a
/// session's observations, besides what [`SessionDigest`] already counts.
///
/// Each field is the answer to a question the full row list used to answer by
/// walking it; the `*_prompt*` and `latest_assistant` fields hold the first or
/// last row that passes the same predicate, found by an early-exit scan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HandoffFacts {
    /// First user prompt with usable text (not a harness markup block).
    pub(crate) first_prompt: Option<String>,
    /// Last user prompt with usable text.
    pub(crate) last_prompt: Option<String>,
    /// Last user prompt that is not a markup block, even when its text is
    /// empty: the one the open-question heuristics test for a question.
    pub(crate) question_prompt: Option<PromptFact>,
    /// Title of the last completed tool call.
    pub(crate) last_tool_title: Option<String>,
    /// The newest failed call.
    pub(crate) last_failed_tool: Option<FailedToolFact>,
    /// Body of the last `Stop` that carries assistant text.
    pub(crate) latest_assistant: Option<String>,
    /// The session recorded a `Stop`.
    pub(crate) has_stop: bool,
    /// The session recorded a `SessionEnd`.
    pub(crate) has_session_end: bool,
    /// Tool names worth listing in the baton: a harness's own tool name, not
    /// a `tool <family>` label.
    pub(crate) tools: BTreeSet<String>,
}

/// What a session's page and handoff are rendered from.
///
/// Built by [`Self::load`] with bounded reads in one read transaction, so a
/// session of any size costs the same memory apart from what the output
/// itself lists: the first and last prompt-title slice the page prints, at
/// most [`PROMPT_TITLES_MAX`] titles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDigest {
    /// Rows the session holds.
    total: usize,
    /// At least one prompt or tool use (the page is not lifecycle-only).
    substantive: bool,
    /// The first raw-observation lines the page shows (all of them when the
    /// session fits in [`RAW_OBSERVATION_MAX_LINES`]).
    first_lines: Vec<SessionLine>,
    /// The last lines, only for a session longer than the cap.
    last_lines: Vec<SessionLine>,
    /// When the newest `SessionStart` happened.
    start_at: Option<Timestamp>,
    /// The newest `SessionEnd`.
    end: Option<SessionEndFact>,
    /// The prompt titles the page lists: all of them when the session holds
    /// at most [`PROMPT_TITLES_MAX`], otherwise the first
    /// [`PROMPT_TITLES_HEAD`] and the last [`PROMPT_TITLES_TAIL`], each in
    /// capture order.
    prompt_titles: Vec<String>,
    /// User prompts the session holds (the true total; the list above is
    /// bounded).
    prompt_count: usize,
    /// Completed calls per tool name, keyed for stable ordering.
    tool_counts: BTreeMap<String, usize>,
    /// Calls the harness reported as failed.
    failed_tool_calls: usize,
    /// The page title.
    title: String,
    /// What the handoff and the abrupt-ending note read.
    handoff: HandoffFacts,
}

/// What loading a [`SessionDigest`] cost, for a bound test and for debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DigestLoadStats {
    /// Rows deserialized: the raw-observation lines plus every row an
    /// early-exit scan visited (one resident at a time).
    pub rows_read: usize,
    /// User prompts the session holds (the digest keeps at most the bounded
    /// slice of their titles).
    pub prompt_titles: usize,
    /// Prompt bodies read to settle the page title (one resident at a time).
    pub prompt_bodies_read: usize,
}

impl SessionDigest {
    /// Read a session's digest with bounded queries.
    ///
    /// # Errors
    /// Propagates any store error.
    pub async fn load(
        reader: &ReaderPool,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) -> StoreResult<Self> {
        Ok(
            Self::load_with_stats(reader, workspace_id, project_id, session_id)
                .await?
                .0,
        )
    }

    /// [`Self::load`], also returning the session's observation count from
    /// the SAME read snapshot the digest was built from.
    ///
    /// The idle auto-end tick binds that count to the end stamp (review R36
    /// F3): an observation landing after the digest was built must invalidate
    /// the stamp instead of reaching the watermark without reaching the page.
    ///
    /// # Errors
    /// Propagates any store error.
    pub(crate) async fn load_with_count(
        reader: &ReaderPool,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) -> StoreResult<(Self, i64)> {
        reader
            .read_session_observations(workspace_id, project_id, session_id, move |snapshot| {
                let (digest, _stats) = build_digest(snapshot, session_id)?;
                // Overflow only with more than 9.2e18 observations: fail
                // toward `Lost` at the stamp (an unrepresentable count never
                // matches the stamp-time COUNT), never toward a wrong stamp.
                let total = i64::try_from(digest.observation_count()).unwrap_or(i64::MAX);
                Ok((digest, total))
            })
            .await
    }

    /// [`Self::load`], also reporting what the load read.
    ///
    /// # Errors
    /// Propagates any store error.
    pub(crate) async fn load_with_stats(
        reader: &ReaderPool,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) -> StoreResult<(Self, DigestLoadStats)> {
        reader
            .read_session_observations(workspace_id, project_id, session_id, move |snapshot| {
                build_digest(snapshot, session_id)
            })
            .await
    }

    /// Whether the session recorded real work (a prompt or a tool use).
    #[must_use]
    pub fn is_substantive(&self) -> bool {
        self.substantive
    }

    /// Rows the session holds.
    #[must_use]
    pub fn observation_count(&self) -> usize {
        self.total
    }

    pub(crate) fn handoff_facts(&self) -> &HandoffFacts {
        &self.handoff
    }

    /// For tests in other modules; the renderers read the field.
    #[cfg(test)]
    pub(crate) fn failed_tool_calls(&self) -> usize {
        self.failed_tool_calls
    }
}

fn micros_to_timestamp(micros: i64) -> StoreResult<Timestamp> {
    Timestamp::from_microsecond(micros).map_err(|e| {
        StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
            "bad timestamp: {e}"
        )))
    })
}

/// The handoff prompt predicate: a user prompt whose text is not empty and not
/// a harness block delivered as a user turn (`<task-notification>`).
fn handoff_prompt(obs: &Observation) -> Option<String> {
    if obs.kind != ObservationKind::UserPrompt {
        return None;
    }
    let text = prompt_text(obs);
    (!text.is_empty() && !looks_like_markup_block(text)).then(|| text.to_string())
}

/// The open-question prompt predicate: a user prompt that is not a harness
/// block. An empty text still counts, as it always did.
fn question_prompt(obs: &Observation) -> Option<PromptFact> {
    if obs.kind != ObservationKind::UserPrompt || looks_like_markup_block(prompt_text(obs)) {
        return None;
    }
    Some(PromptFact {
        title: obs.title.clone(),
        body: obs.body.clone(),
    })
}

/// Stop bodies exist only after the assistant-capture double opt-in.
fn assistant_response(obs: &Observation) -> Option<String> {
    (obs.kind == ObservationKind::Stop && !obs.body.trim().is_empty()).then(|| obs.body.clone())
}

#[cfg(test)]
fn session_line(obs: &Observation) -> SessionLine {
    SessionLine {
        kind: obs.kind,
        extension: obs.extension.clone(),
        source_event: obs.source_event.clone(),
        title: obs
            .title
            .chars()
            .take(ai_memory_store::SESSION_LINE_TITLE_CHARS)
            .collect(),
        created_at: obs.created_at,
    }
}

fn build_digest(
    snapshot: &SessionObservationSnapshot<'_>,
    session_id: SessionId,
) -> StoreResult<(SessionDigest, DigestLoadStats)> {
    let summary: SessionKindSummary = snapshot.kind_summary()?;
    let total = summary.total;

    // The raw-observation list: everything when it fits, else the head and the
    // tail the page prints around its "omitted" marker.
    let (first_lines, last_lines) = if total <= RAW_OBSERVATION_MAX_LINES {
        (snapshot.first_lines(RAW_OBSERVATION_MAX_LINES)?, Vec::new())
    } else {
        (
            snapshot.first_lines(RAW_OBSERVATION_HEAD_LINES)?,
            snapshot.last_lines(RAW_OBSERVATION_TAIL_LINES)?,
        )
    };

    let start_at = summary
        .last_session_start_at
        .map(micros_to_timestamp)
        .transpose()?;
    let end = match summary.last_session_end_at {
        Some(at) => snapshot.scan(
            ObservationOrder::Desc,
            &[ObservationKind::SessionEnd],
            Some(at),
            |obs| {
                Some(SessionEndFact {
                    created_at: obs.created_at,
                    body: obs.body.clone(),
                })
            },
        )?,
        None => None,
    };

    // The prompt titles the page lists: bounded the way the raw-observation
    // list is, with the true prompt count beside them.
    let prompts = snapshot.prompt_titles(PROMPT_TITLES_HEAD, PROMPT_TITLES_TAIL)?;
    let mut prompt_titles = prompts.first;
    prompt_titles.extend(prompts.last);

    // Bodies only for a title that could be the subagent preamble: the title
    // rule is the one reader of a prompt body, and the walk stops once the
    // title is settled.
    let mut title_scan = PromptTitleScan::default();
    let mut prompt_bodies_read = 0;
    snapshot.scan_prompts(SUBAGENT_PROMPT_PREAMBLE, |title, body| {
        prompt_bodies_read += usize::from(body.is_some());
        title_scan.push(title, body.unwrap_or(""))
    })?;
    let title = match title_scan.finish() {
        PromptTitle::Found(title) => title,
        PromptTitle::RejectedPreamble => format!("Session {session_id}"),
        PromptTitle::Unusable => snapshot
            .scan(ObservationOrder::Asc, &[], None, fallback_title)?
            .unwrap_or_else(|| title_without_candidate(session_id, total)),
    };

    let tools = snapshot.tool_titles()?;
    let mut tool_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (title, count) in tools.completed {
        *tool_counts
            .entry(tool_title_display(&title).to_owned())
            .or_insert(0) += count;
    }

    let handoff = HandoffFacts {
        first_prompt: if summary.prompts > 0 {
            snapshot.scan(
                ObservationOrder::Asc,
                &[ObservationKind::UserPrompt],
                None,
                handoff_prompt,
            )?
        } else {
            None
        },
        last_prompt: snapshot.scan(
            ObservationOrder::Desc,
            &[ObservationKind::UserPrompt],
            summary.last_prompt_at,
            handoff_prompt,
        )?,
        question_prompt: snapshot.scan(
            ObservationOrder::Desc,
            &[ObservationKind::UserPrompt],
            summary.last_prompt_at,
            question_prompt,
        )?,
        last_tool_title: snapshot.scan(
            ObservationOrder::Desc,
            &[ObservationKind::PostToolUse],
            summary.last_post_tool_at,
            |obs| Some(obs.title.clone()),
        )?,
        last_failed_tool: match summary.last_failed_tool_at {
            Some(at) => snapshot.scan(
                ObservationOrder::Desc,
                &[ObservationKind::PostToolUse],
                Some(at),
                failed_tool_fact,
            )?,
            None => None,
        },
        latest_assistant: match summary.last_stop_with_body_at {
            Some(at) => snapshot.scan(
                ObservationOrder::Desc,
                &[ObservationKind::Stop],
                Some(at),
                assistant_response,
            )?,
            None => None,
        },
        has_stop: summary.last_stop_at.is_some(),
        has_session_end: summary.last_session_end_at.is_some(),
        tools: tools
            .titles
            .iter()
            .filter_map(|title| listable_tool_name(title))
            .map(str::to_owned)
            .collect(),
    };

    let stats = DigestLoadStats {
        rows_read: snapshot.rows_read(),
        prompt_titles: prompts.total,
        prompt_bodies_read,
    };
    Ok((
        SessionDigest {
            total,
            substantive: summary.substantive,
            first_lines,
            last_lines,
            start_at,
            end,
            prompt_titles,
            prompt_count: prompts.total,
            tool_counts,
            failed_tool_calls: summary.failed_tool_calls,
            title,
            handoff,
        },
        stats,
    ))
}

/// What the prompts alone say about the title.
enum PromptTitle {
    /// A usable prompt (or the body of a subagent preamble) named the page.
    Found(String),
    /// No prompt was usable, and at least one was the subagent preamble.
    RejectedPreamble,
    /// No prompt was usable: fall back to the other observations.
    Unusable,
}

/// [`PromptTitleScan`] over a whole prompt list: the row-list form the
/// test-only digest builder walks.
#[cfg(test)]
fn title_from_prompts<'a>(prompts: impl Iterator<Item = (&'a str, &'a str)>) -> PromptTitle {
    let mut scan = PromptTitleScan::default();
    for (title, body) in prompts {
        if !scan.push(title, body) {
            break;
        }
    }
    scan.finish()
}

/// Title for a synthesised session page.
///
/// Prefers the first user prompt that reads as something a person wrote.
/// A prompt payload is whatever the harness put there, so IDE context
/// blocks and an echoed shell prompt can arrive in the same field (#484) —
/// those are skipped rather than becoming the title the page is indexed and
/// displayed under.
///
/// A `SessionStart` is skipped as a source outright. `best_title_hint`
/// fills its title from `model` before `title` (`payload.rs`), so a bare
/// model id became the page title whenever the prompts were unusable — and
/// by the time it is a string it is indistinguishable from a terse human
/// reference like `pr-477`, so it is excluded here by kind instead.
///
/// The skipped field is `["model", "title"]`, so this drops more than a
/// model id: OpenCode's `session.created` puts `info.title` in the same
/// slot. Its plugin subscribes to `created`/`idle`/`deleted`/`compacted`
/// and never `session.updated`, and starts a session once per id, so the
/// only value that can arrive is the creation-time name — in practice the
/// shared literal "New session", never a rename. **That last part is an
/// assumption about an external component, not an invariant:** it is read
/// off OpenCode's subscription list as of v1.32.1, and if a future release
/// surfaces a renamed session here, this would discard a good title with
/// nothing in the tree failing. What a `SessionStart` carries is whatever
/// the harness *named the session*, and no harness names it with something
/// a person typed.
///
/// Falls through in decreasing order of confidence: a usable prompt, then
/// any other usable observation title, then the session's own path. The
/// literal "session" remains only for the case where there is nothing else
/// at all.
///
/// Fed one prompt at a time, so a caller streaming prompts from the store
/// holds one body at most and stops reading bodies once the title is settled.
#[derive(Default)]
struct PromptTitleScan {
    found: Option<String>,
    rejected_subagent_preamble: bool,
}

impl PromptTitleScan {
    /// Feed the next prompt in capture order. Returns `false` once the title
    /// is found: later prompts cannot change it.
    fn push(&mut self, title: &str, body: &str) -> bool {
        if title.trim() == SUBAGENT_PROMPT_PREAMBLE {
            self.rejected_subagent_preamble = true;
            if let Some(line) = body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !looks_like_title_scaffolding(line))
            {
                self.found = Some(truncate_for_title(line));
                return false;
            }
            return true;
        }
        if !title.is_empty() && !looks_like_scaffolding(title) {
            self.found = Some(title.to_string());
            return false;
        }
        true
    }

    fn finish(self) -> PromptTitle {
        match self.found {
            Some(title) => PromptTitle::Found(title),
            None if self.rejected_subagent_preamble => PromptTitle::RejectedPreamble,
            None => PromptTitle::Unusable,
        }
    }
}

/// The title a non-prompt observation can lend the page, in capture order.
fn fallback_title(obs: &Observation) -> Option<String> {
    // The router titles an untitled lifecycle event with its kind name
    // (`stop`, `session-end`): a string every such page would share, and
    // what this loop reached once tool-family labels were skipped (#895).
    if obs.kind == ObservationKind::SessionStart || obs.title == obs.kind.as_str() {
        return None;
    }
    // A `tool <family>` label is a partition of the calls, not a title:
    // `looks_like_scaffolding` does not catch it, so reject it here or a
    // session whose only non-prompt observation is a closed-tool call
    // ("tool non-file") would take the label as its page title.
    (!obs.title.is_empty()
        && !looks_like_title_scaffolding(&obs.title)
        && !is_safe_tool_title(&obs.title))
    .then(|| obs.title.clone())
}

/// Every candidate was scaffolding. The page still needs a title, and its
/// own identity is more use to a reader than the word "session" repeated
/// across every such page.
fn title_without_candidate(session_id: SessionId, total: usize) -> String {
    if total == 0 {
        return "session".to_string();
    }
    format!("Session {session_id}")
}

fn looks_like_title_scaffolding(candidate: &str) -> bool {
    candidate.trim() == SUBAGENT_PROMPT_PREAMBLE || looks_like_scaffolding(candidate)
}

/// How many tools to name individually before summarising the rest as a count.
const SUMMARY_MAX_NAMED_TOOLS: usize = 3;

/// One line of plain prose describing what the session did, or empty when it
/// did nothing worth stating.
///
/// This lands in `frontmatter.summary`, which the store prefers over the body
/// when building the descriptor for a non-FTS hit. It is **not** used
/// verbatim: the reader runs it through the same line filter it applies to a
/// body, which drops headings, `- **key:** value` metadata bullets, and any
/// line that merely repeats the title. When every line is dropped the filter
/// falls back to echoing its raw input, so a summary written in the same house
/// style as the metadata block above would be reproduced verbatim as the
/// descriptor — displacing the body text that would otherwise have been used,
/// silently and with no error anywhere.
///
/// Hence: one line, plain prose, no leading marker.
fn session_summary(digest: &SessionDigest) -> String {
    let mut parts: Vec<String> = Vec::new();

    if digest.prompt_count > 0 {
        parts.push(format!(
            "{} prompt{}",
            digest.prompt_count,
            plural(digest.prompt_count)
        ));
    }

    let calls: usize = digest.tool_counts.values().sum();
    if calls > 0 {
        // Name the busiest tools first; ties fall back to the map's
        // alphabetical order so the same session always renders the same way.
        let mut by_calls: Vec<(&str, usize)> = digest
            .tool_counts
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        by_calls.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        // A `tool <family>` title is what `safe_tool_title` writes when the
        // harness's own tool name is not carried through: a partition of the
        // calls, never a name for them. Listing those spends the reader's
        // attention to say "some of the calls touched files and some did
        // not", so they are counted but not named, and a session with
        // nothing else to name drops the clause rather than filling it.
        let nameable: Vec<&str> = by_calls
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !is_safe_tool_title(name))
            .collect();
        parts.push(if nameable.is_empty() {
            format!("{calls} completed tool call{}", plural(calls))
        } else {
            let named: Vec<&str> = nameable
                .iter()
                .copied()
                .take(SUMMARY_MAX_NAMED_TOOLS)
                .collect();
            let tools = if by_calls.len() > named.len() {
                format!(
                    "{} and {} more",
                    join_and(&named),
                    by_calls.len() - named.len()
                )
            } else {
                join_and(&named)
            };
            format!(
                "{calls} completed tool call{} across {tools}",
                plural(calls)
            )
        });
    }
    if digest.failed_tool_calls > 0 {
        parts.push(format!("{} failed", digest.failed_tool_calls));
    }

    if let (Some(start), Some(end)) = (digest.start_at, digest.end.as_ref())
        && let Some(spent) = format_duration(end.created_at.as_second() - start.as_second())
    {
        parts.push(format!("over {spent}"));
    }

    // The harness's own word for why the session ended (#1225), when the
    // SessionEnd observation carried one. Display-bounded: a summary line
    // is no place for a long free-text field.
    if let Some(reason) = digest
        .end
        .as_ref()
        .and_then(|end| end.body.strip_prefix("reason: "))
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        let shown: String = reason.chars().take(60).collect();
        parts.push(format!("ended ({shown})"));
    }

    if parts.is_empty() {
        return String::new();
    }
    format!("{}.", parts.join(", "))
}

/// `""` or `"s"` for a count.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `"Bash"`, `"Bash and Edit"`, `"Bash, Edit and Read"`.
fn join_and(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Human-readable elapsed time, or `None` when the session was not measurably
/// long (a clock that did not advance, or ran backwards).
fn format_duration(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    if seconds < 60 {
        return Some(format!("{seconds}s"));
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return Some(format!("{minutes}m"));
    }
    let (hours, rest) = (minutes / 60, minutes % 60);
    Some(if rest == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rest}m")
    })
}

fn render_body(session_id: SessionId, digest: &SessionDigest, title: &str) -> String {
    let mut buf = String::with_capacity(2048);
    buf.push_str(&format!("# {title}\n\n"));

    buf.push_str("## Session metadata\n\n");
    buf.push_str(&format!("- **session_id:** `{session_id}`\n"));
    if let Some(s) = &digest.start_at {
        buf.push_str(&format!("- **started_at:** {}\n", human_ts(s)));
    }
    if let Some(e) = &digest.end {
        buf.push_str(&format!("- **ended_at:** {}\n", human_ts(&e.created_at)));
    }
    buf.push_str(&format!("- **observations:** {}\n\n", digest.total));

    if let Some(note) = abrupt_ending_note(&digest.handoff) {
        buf.push_str(&format!("{note}\n\n"));
    }

    if digest.prompt_count > 0 {
        buf.push_str("## Prompts\n\n");
        if digest.prompt_count <= PROMPT_TITLES_MAX {
            for (i, title) in digest.prompt_titles.iter().enumerate() {
                buf.push_str(&format!("{}. {title}\n", i + 1));
            }
        } else {
            let (head, tail) = digest.prompt_titles.split_at(PROMPT_TITLES_HEAD);
            for (i, title) in head.iter().enumerate() {
                buf.push_str(&format!("{}. {title}\n", i + 1));
            }
            let omitted = digest.prompt_count - PROMPT_TITLES_HEAD - PROMPT_TITLES_TAIL;
            buf.push_str(&format!(
                "\n_... {omitted} prompts omitted from the middle (showing first {PROMPT_TITLES_HEAD} and last {PROMPT_TITLES_TAIL})._\n\n",
            ));
            for (i, title) in tail.iter().enumerate() {
                buf.push_str(&format!(
                    "{}. {title}\n",
                    digest.prompt_count - PROMPT_TITLES_TAIL + i + 1
                ));
            }
        }
        buf.push('\n');
    }

    if !digest.tool_counts.is_empty() {
        buf.push_str("## Tool calls\n\n");
        for (name, count) in &digest.tool_counts {
            buf.push_str(&format!("- `{name}`: {count}\n"));
        }
        buf.push('\n');
    }

    // What broke: the count and the newest error text.
    if digest.failed_tool_calls > 0 {
        buf.push_str("## Failed tool calls\n\n");
        buf.push_str(&format!("- **count:** {}\n", digest.failed_tool_calls));
        if let Some(failed) = &digest.handoff.last_failed_tool {
            buf.push_str(&format!("- **last:** {}\n", failed.describe()));
        }
        buf.push('\n');
    }

    buf.push_str("## Raw observations\n\n");
    render_raw_observations(&mut buf, digest);

    buf.push_str("\n_Synthesised by ai-memory (M3, no-LLM heuristic)._\n");
    buf
}

/// One deterministic line flagging an abrupt ending, or `None` when the
/// session closed normally (a Stop and a SessionEnd are both on record).
///
/// Mirrors the multi-signal heuristics the router's `derive_open_questions`
/// uses for automatic SessionEnd handoffs, first match wins, so the
/// zero-LLM page states the same terminal state a handoff would flag
/// (#1221):
/// 1. The final substantive prompt is an unanswered question and no Stop
///    ever arrived after it.
/// 2. The last completed tool call touched files and no Stop followed —
///    the session ended while working in the tree.
/// 3. A Stop was recorded but no SessionEnd — a mid-task exit (or a
///    checkpoint page rendered while the session is still running; the
///    wording only claims what the log shows).
fn abrupt_ending_note(facts: &HandoffFacts) -> Option<&'static str> {
    if !facts.has_stop
        && let Some(prompt) = &facts.question_prompt
    {
        let text = if prompt.body.is_empty() {
            prompt.title.trim()
        } else {
            prompt.body.trim()
        };
        if text.ends_with('?') || text.ends_with('？') {
            return Some(
                "Session ended without a normal stop; the final prompt was never answered.",
            );
        }
    }

    if !facts.has_stop
        && let Some(tool) = &facts.last_tool_title
        && tool_family_from_title(tool) == Some(ToolFamily::File)
    {
        return Some(
            "Session ended without a normal stop while working with files; \
             the working tree may hold uncommitted or unverified changes.",
        );
    }

    if facts.has_stop && !facts.has_session_end {
        return Some(
            "A stop was recorded but no session end; the session may have exited mid-task.",
        );
    }

    None
}

fn render_raw_observations(buf: &mut String, digest: &SessionDigest) {
    for line in &digest.first_lines {
        render_raw_observation(buf, line);
    }
    if digest.total <= RAW_OBSERVATION_MAX_LINES {
        return;
    }
    let omitted = digest.total - RAW_OBSERVATION_HEAD_LINES - RAW_OBSERVATION_TAIL_LINES;
    buf.push_str(&format!(
        "\n_... {omitted} raw observations omitted from the middle (showing first {RAW_OBSERVATION_HEAD_LINES} and last {RAW_OBSERVATION_TAIL_LINES})._\n\n",
    ));
    for line in &digest.last_lines {
        render_raw_observation(buf, line);
    }
}

fn render_raw_observation(buf: &mut String, line: &SessionLine) {
    let kind = observation_kind_label(line);
    buf.push_str(&format!(
        "- `{}` @ {} — {}\n",
        kind,
        human_ts(&line.created_at),
        line.title,
    ));
}

fn observation_kind_label(line: &SessionLine) -> String {
    match (&line.extension, &line.source_event) {
        (Some(extension), Some(source_event)) => {
            format!("{} [{}:{}]", line.kind.as_str(), extension, source_event)
        }
        _ => line.kind.as_str().to_string(),
    }
}

fn human_ts(ts: &jiff::Timestamp) -> String {
    ts.to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// The digest a full row list gives, computed by walking it with the same
/// predicates the bounded queries apply. Test-only: production reads the
/// database, and the equivalence tests pin this walk and the queries together.
#[cfg(test)]
impl SessionDigest {
    pub(crate) fn from_observations(session_id: SessionId, observations: &[Observation]) -> Self {
        let total = observations.len();
        let (first_lines, last_lines) = if total <= RAW_OBSERVATION_MAX_LINES {
            (observations.iter().map(session_line).collect(), Vec::new())
        } else {
            (
                observations[..RAW_OBSERVATION_HEAD_LINES]
                    .iter()
                    .map(session_line)
                    .collect(),
                observations[total - RAW_OBSERVATION_TAIL_LINES..]
                    .iter()
                    .map(session_line)
                    .collect(),
            )
        };
        let last_of = |kind: ObservationKind| observations.iter().rev().find(|o| o.kind == kind);
        let title = match title_from_prompts(
            observations
                .iter()
                .filter(|o| o.kind == ObservationKind::UserPrompt)
                .map(|o| (o.title.as_str(), o.body.as_str())),
        ) {
            PromptTitle::Found(title) => title,
            PromptTitle::RejectedPreamble => format!("Session {session_id}"),
            PromptTitle::Unusable => observations
                .iter()
                .find_map(fallback_title)
                .unwrap_or_else(|| title_without_candidate(session_id, total)),
        };
        let mut tool_counts = BTreeMap::new();
        for obs in observations {
            if obs.kind == ObservationKind::PostToolUse && !obs.title.is_empty() {
                *tool_counts
                    .entry(tool_title_display(&obs.title).to_owned())
                    .or_insert(0) += 1;
            }
        }
        let prompt_titles_all: Vec<String> = observations
            .iter()
            .filter(|o| o.kind == ObservationKind::UserPrompt)
            .map(|o| o.title.clone())
            .collect();
        // The store's bounded read keeps every title when the session fits in
        // the cap and the first/last slice when it does not; mirror that here
        // so the row-list digest and the database digest stay equal.
        let (prompt_count, prompt_titles) = if prompt_titles_all.len() <= PROMPT_TITLES_MAX {
            (prompt_titles_all.len(), prompt_titles_all)
        } else {
            (
                prompt_titles_all.len(),
                prompt_titles_all[..PROMPT_TITLES_HEAD]
                    .iter()
                    .chain(prompt_titles_all[prompt_titles_all.len() - PROMPT_TITLES_TAIL..].iter())
                    .cloned()
                    .collect(),
            )
        };
        Self {
            total,
            substantive: observations.iter().any(|o| {
                matches!(
                    o.kind,
                    ObservationKind::UserPrompt
                        | ObservationKind::PreToolUse
                        | ObservationKind::PostToolUse
                )
            }),
            first_lines,
            last_lines,
            start_at: last_of(ObservationKind::SessionStart).map(|o| o.created_at),
            end: last_of(ObservationKind::SessionEnd).map(|o| SessionEndFact {
                created_at: o.created_at,
                body: o.body.clone(),
            }),
            prompt_titles,
            prompt_count,
            tool_counts,
            failed_tool_calls: observations.iter().filter(|o| is_failed_tool(o)).count(),
            title,
            handoff: HandoffFacts::from_observations(observations),
        }
    }
}

#[cfg(test)]
impl HandoffFacts {
    pub(crate) fn from_observations(observations: &[Observation]) -> Self {
        Self {
            first_prompt: observations.iter().find_map(handoff_prompt),
            last_prompt: observations.iter().rev().find_map(handoff_prompt),
            question_prompt: observations.iter().rev().find_map(question_prompt),
            last_tool_title: observations
                .iter()
                .rev()
                .find(|o| o.kind == ObservationKind::PostToolUse)
                .map(|o| o.title.clone()),
            last_failed_tool: observations.iter().rev().find_map(failed_tool_fact),
            latest_assistant: observations.iter().rev().find_map(assistant_response),
            has_stop: observations.iter().any(|o| o.kind == ObservationKind::Stop),
            has_session_end: observations
                .iter()
                .any(|o| o.kind == ObservationKind::SessionEnd),
            tools: observations
                .iter()
                .filter(|o| {
                    matches!(
                        o.kind,
                        ObservationKind::PostToolUse | ObservationKind::PreToolUse
                    )
                })
                .filter_map(|o| listable_tool_name(&o.title))
                .map(str::to_owned)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One id per call is fine: assertions here never depend on its value,
    /// only on whether the fallback path was taken at all.
    fn test_session_id() -> SessionId {
        SessionId::new()
    }
    use ai_memory_core::{ObservationId, SessionId};
    use jiff::Timestamp;

    // The renderers take a digest of the session. These tests describe a
    // session as the row list it is made of, so each shim digests that list
    // the way the database queries do (`SessionDigest::from_observations`),
    // and the assertions below read as they always did.
    fn synthesize_session_page(
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        agent_kind: AgentKind,
        observations: &[Observation],
    ) -> NewPage {
        super::synthesize_session_page(
            workspace_id,
            project_id,
            session_id,
            agent_kind,
            &SessionDigest::from_observations(session_id, observations),
        )
    }

    fn tally_session(observations: &[Observation]) -> SessionDigest {
        SessionDigest::from_observations(SessionId::new(), observations)
    }

    fn derive_title(observations: &[Observation], session_id: SessionId) -> String {
        SessionDigest::from_observations(session_id, observations).title
    }

    fn obs(kind: ObservationKind, title: &str) -> Observation {
        Observation {
            id: ObservationId::new(),
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind,
            extension: None,
            source_event: None,
            title: title.into(),
            body: String::new(),
            importance: 5,
            created_at: Timestamp::now(),
        }
    }

    fn obs_at(kind: ObservationKind, title: &str, at: &str) -> Observation {
        let mut o = obs(kind, title);
        o.created_at = at.parse::<Timestamp>().expect("fixture timestamp");
        o
    }

    fn obs_with_body(kind: ObservationKind, title: &str, body: &str) -> Observation {
        let mut o = obs(kind, title);
        o.body = body.into();
        o
    }

    #[test]
    fn the_summary_states_the_shape_of_the_session() {
        let observations = vec![
            obs_at(
                ObservationKind::SessionStart,
                "session",
                "2026-08-24T10:00:00Z",
            ),
            obs(ObservationKind::UserPrompt, "bound the scheduler queue"),
            obs(ObservationKind::UserPrompt, "make the test deterministic"),
            // Pre+Post pairs: three completed calls, not six.
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PostToolUse, "Bash"),
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PostToolUse, "Bash"),
            obs(ObservationKind::PreToolUse, "Edit"),
            obs(ObservationKind::PostToolUse, "Edit"),
            obs_at(
                ObservationKind::SessionEnd,
                "session",
                "2026-08-24T10:18:00Z",
            ),
        ];
        assert_eq!(
            session_summary(&tally_session(&observations)),
            "2 prompts, 3 completed tool calls across Bash and Edit, over 18m."
        );
    }

    /// #1225: the harness's end reason, when captured, closes the summary;
    /// a long one is display-bounded.
    #[test]
    fn the_summary_states_why_the_session_ended() {
        let end = |body: &str| {
            let mut o = obs_at(
                ObservationKind::SessionEnd,
                "session",
                "2026-08-24T10:05:00Z",
            );
            o.body = body.into();
            o
        };
        let start = obs_at(
            ObservationKind::SessionStart,
            "session",
            "2026-08-24T10:00:00Z",
        );
        let prompt = obs(ObservationKind::UserPrompt, "ship it");
        assert_eq!(
            session_summary(&tally_session(&[
                start.clone(),
                prompt.clone(),
                end("reason: logout")
            ])),
            "1 prompt, over 5m, ended (logout)."
        );
        assert_eq!(
            session_summary(&tally_session(&[start.clone(), prompt.clone(), end("")])),
            "1 prompt, over 5m."
        );
        let long = format!("reason: {}", "x".repeat(200));
        let summary = session_summary(&tally_session(&[start, prompt, end(&long)]));
        assert!(
            summary.ends_with(&format!("ended ({}).", "x".repeat(60))),
            "{summary}"
        );
    }

    #[test]
    fn the_summary_is_shaped_so_the_reader_does_not_drop_it() {
        // The store prefers `summary` over the body and then runs it through
        // the same line filter it applies to a body: headings, `- **key:**
        // value` metadata bullets, and lines repeating the title are all
        // dropped, and when nothing survives it echoes its raw input. A
        // summary shaped like the metadata block this very module renders
        // would therefore be reproduced verbatim as the descriptor, in place
        // of body text that would have been usable. Assert the shape that
        // keeps that from happening.
        let observations = vec![
            obs_at(
                ObservationKind::SessionStart,
                "session",
                "2026-08-24T10:00:00Z",
            ),
            obs(ObservationKind::UserPrompt, "bound the scheduler queue"),
            obs(ObservationKind::PostToolUse, "Bash"),
            obs_at(
                ObservationKind::SessionEnd,
                "session",
                "2026-08-24T10:05:00Z",
            ),
        ];
        let summary = session_summary(&tally_session(&observations));

        assert!(!summary.contains('\n'), "must stay one line: {summary:?}");
        assert!(
            !summary.starts_with('#')
                && !summary.starts_with("---")
                && !summary.starts_with("___")
                && !summary.starts_with("***"),
            "must not read as a structural line: {summary:?}"
        );
        assert!(
            !(summary.starts_with("- **") && summary.contains(":**")),
            "must not read as a metadata bullet: {summary:?}"
        );
        assert!(
            !summary.starts_with("- ") && !summary.starts_with("* ") && !summary.starts_with("+ "),
            "must not open with a list marker: {summary:?}"
        );
        assert_ne!(
            summary,
            derive_title(&observations, test_session_id()),
            "a summary equal to the title is dropped as a repeat"
        );
    }

    #[test]
    fn a_session_with_nothing_to_report_gets_no_summary() {
        // An empty summary must stay out of the frontmatter entirely: a blank
        // one still wins the store's COALESCE and would cost the page its
        // body-derived descriptor.
        // Fixed, equal timestamps: `obs` stamps `Timestamp::now()`, so two
        // calls straddling a second boundary would make this a session that
        // lasted `1s` and give it a summary after all.
        let lifecycle_only = vec![
            obs_at(
                ObservationKind::SessionStart,
                "session-start",
                "2026-08-24T10:00:00Z",
            ),
            obs_at(
                ObservationKind::SessionEnd,
                "session-end",
                "2026-08-24T10:00:00Z",
            ),
        ];
        assert_eq!(session_summary(&tally_session(&lifecycle_only)), "");

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &lifecycle_only,
        );
        assert!(page.frontmatter_json.get("summary").is_none());
    }

    #[test]
    fn the_synthesised_page_carries_the_summary() {
        let observations = vec![
            obs_at(
                ObservationKind::SessionStart,
                "session",
                "2026-08-24T10:00:00Z",
            ),
            obs(ObservationKind::UserPrompt, "bound the scheduler queue"),
            obs(ObservationKind::PostToolUse, "Bash"),
            obs_at(
                ObservationKind::SessionEnd,
                "session",
                "2026-08-24T10:05:00Z",
            ),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert_eq!(page.frontmatter_json["agent"], "codex");
        assert_eq!(
            page.frontmatter_json["summary"],
            serde_json::json!("1 prompt, 1 completed tool call across Bash, over 5m.")
        );
    }

    /// Measured on a live 1,885-page instance: 21,136 of 29,804 `PostToolUse`
    /// observations (71%) carried one of three `tool <family>` literals,
    /// against 56 real tool names in the other 29%. Every tool mention in
    /// every summary on that instance was a family label, so the clause was
    /// spending characters to say nothing.
    #[test]
    fn family_labels_are_counted_but_not_named() {
        let mut observations = vec![obs(ObservationKind::UserPrompt, "do the thing")];
        for (tool, calls) in [("tool non-file", 9), ("tool file", 4), ("tool unknown", 2)] {
            for _ in 0..calls {
                observations.push(obs(ObservationKind::PostToolUse, tool));
            }
        }
        let summary = session_summary(&tally_session(&observations));
        assert_eq!(summary, "1 prompt, 15 completed tool calls.");
        assert!(
            !summary.contains("across"),
            "nothing nameable is left, so the clause must go rather than be filled: {summary}"
        );
    }

    /// A session that mixes both keeps the names it has. The families still
    /// count toward the total and toward "and N more", because they are real
    /// groups of calls — they just are not worth a reader's attention.
    #[test]
    fn real_names_survive_alongside_family_labels() {
        let mut observations = vec![obs(ObservationKind::UserPrompt, "do the thing")];
        for (tool, calls) in [("tool non-file", 20), ("Bash", 5), ("Edit", 2)] {
            for _ in 0..calls {
                observations.push(obs(ObservationKind::PostToolUse, tool));
            }
        }
        assert_eq!(
            session_summary(&tally_session(&observations)),
            "1 prompt, 27 completed tool calls across Bash and Edit and 1 more."
        );
    }

    #[test]
    fn many_tools_are_named_by_volume_then_counted() {
        let mut observations = vec![obs(ObservationKind::UserPrompt, "do the thing")];
        for (tool, calls) in [
            ("Bash", 5),
            ("Edit", 4),
            ("Read", 3),
            ("Grep", 2),
            ("Glob", 1),
        ] {
            for _ in 0..calls {
                observations.push(obs(ObservationKind::PostToolUse, tool));
            }
        }
        assert_eq!(
            session_summary(&tally_session(&observations)),
            "1 prompt, 15 completed tool calls across Bash, Edit and Read and 2 more."
        );
    }

    /// The #484 defect: the three classes measured across a live corpus must
    /// not become titles, and the page must still get a usable one.
    #[test]
    fn harness_scaffolding_does_not_become_the_title() {
        for scaffold in [
            "<ide_opened_file>The user opened the file /home/samir/x/main.rs",
            "\u{250c}\u{2500}[samir@samirb3 12:56:36] ~/x/ai-usagebar",
        ] {
            let candidates = vec![
                obs(ObservationKind::UserPrompt, scaffold),
                obs(
                    ObservationKind::UserPrompt,
                    "Make the backpressure test deterministic",
                ),
            ];
            assert_eq!(
                derive_title(&candidates, test_session_id()),
                "Make the backpressure test deterministic",
                "{scaffold:?} must be skipped in favour of the next real prompt"
            );
        }
    }

    #[test]
    fn subagent_preamble_promotes_the_first_task_line() {
        let preamble = "You are a subagent spawned by another session.";
        let task = "Review the audit runbook and identify stale checks.";
        let mut prompt = obs(ObservationKind::UserPrompt, preamble);
        prompt.body = format!("{preamble}\n\n{task}");

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::OpenCode,
            &[prompt],
        );

        assert_eq!(page.title, task);
    }

    #[test]
    fn subagent_preamble_without_a_task_falls_back_to_session_identity() {
        let session_id = SessionId::new();
        let preamble = "You are a subagent spawned by another session.";
        let mut prompt = obs(ObservationKind::UserPrompt, preamble);
        prompt.body = preamble.into();
        let observations = vec![prompt, obs(ObservationKind::PostToolUse, "tool file")];

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            session_id,
            AgentKind::OpenCode,
            &observations,
        );

        assert_eq!(page.title, format!("Session {session_id}"));
    }

    #[test]
    fn repeated_user_requests_remain_separate_session_pages() {
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let observations = vec![obs(ObservationKind::UserPrompt, "Review the audit runbook")];

        let first = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            first_session,
            AgentKind::OpenCode,
            &observations,
        );
        let second = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            second_session,
            AgentKind::OpenCode,
            &observations,
        );

        assert_eq!(first.title, second.title);
        assert_ne!(first.path, second.path);
    }

    /// Every candidate is scaffolding. The page still needs a title, and
    /// "session" repeated across every such page is not one.
    #[test]
    fn an_all_scaffolding_session_falls_back_to_its_identity() {
        let sid = test_session_id();
        // The model id arrives on the `SessionStart`, which is where
        // `best_title_hint` puts the harness's `model` field — not as a
        // prompt somebody typed.
        let candidates = vec![
            obs(ObservationKind::SessionStart, "claude-opus-5[1m]"),
            obs(ObservationKind::UserPrompt, "<ide_opened_file>x"),
        ];
        let title = derive_title(&candidates, sid);
        assert_eq!(title, format!("Session {sid}"));
        assert_ne!(title, "session", "must not collapse to a shared literal");
    }

    /// The shape measured in production on 2026-08-21: a 17-observation
    /// session whose `UserPrompt` carried an **empty** title, so the fallback
    /// loop reached the `SessionStart` and the harness's model id became the
    /// page title. The prompt was never scaffolding — it was absent, which is
    /// why no filter on the string could have caught this.
    ///
    /// A `tool <family>` label (`safe_tool_title`) is not a title either: it
    /// is a partition of the calls, so the fallback loop must skip it and fall
    /// through to the session's own identity, never surface "tool non-file" as
    /// the page title (#895).
    /// The shape a real SessionEnd synthesises from: the router stores the
    /// kind name as the title of a lifecycle event that carries none
    /// (`title_hint.unwrap_or(kind)`), so once the tool labels are skipped the
    /// fallback reached `stop`, or `session-end` when no Stop was captured.
    #[test]
    fn a_labels_only_session_does_not_fall_through_to_a_lifecycle_literal() {
        let sid = test_session_id();
        let with_stop = vec![
            obs(ObservationKind::SessionStart, "claude-opus-5[1m]"),
            obs(ObservationKind::UserPrompt, ""),
            obs(ObservationKind::PreToolUse, "tool non-file"),
            obs(ObservationKind::PostToolUse, "tool non-file"),
            obs(ObservationKind::Stop, ObservationKind::Stop.as_str()),
            obs(
                ObservationKind::SessionEnd,
                ObservationKind::SessionEnd.as_str(),
            ),
        ];
        assert_eq!(derive_title(&with_stop, sid), format!("Session {sid}"));
        let without_stop: Vec<_> = with_stop
            .into_iter()
            .filter(|o| o.kind != ObservationKind::Stop)
            .collect();
        assert_eq!(derive_title(&without_stop, sid), format!("Session {sid}"));
    }

    #[test]
    fn a_model_id_on_session_start_never_becomes_the_title() {
        let sid = test_session_id();
        let with_only_a_tool_family_label = vec![
            obs(ObservationKind::SessionStart, "claude-opus-5[1m]"),
            obs(ObservationKind::UserPrompt, ""),
            obs(ObservationKind::PostToolUse, "tool non-file"),
        ];
        assert_eq!(
            derive_title(&with_only_a_tool_family_label, sid),
            format!("Session {sid}"),
            "a tool-family label is not a page title"
        );

        let nothing_else = vec![
            obs(ObservationKind::SessionStart, "claude-opus-5[1m]"),
            obs(ObservationKind::UserPrompt, ""),
        ];
        assert_eq!(
            derive_title(&nothing_else, sid),
            format!("Session {sid}"),
            "the session's identity, never the model it ran on"
        );
    }

    /// Skipping the kind also excludes what the router writes when a harness
    /// sends neither `model` nor `title`: `title_hint.unwrap_or(kind)`, i.e.
    /// the literal "session-start". The shape predicate never caught that,
    /// and it is a string every such page would share.
    #[test]
    fn the_routers_default_session_start_title_is_not_a_page_title() {
        let sid = test_session_id();
        for stored in ["session-start", "New session"] {
            let candidates = vec![
                obs(ObservationKind::SessionStart, stored),
                obs(ObservationKind::UserPrompt, ""),
            ];
            assert_eq!(
                derive_title(&candidates, sid),
                format!("Session {sid}"),
                "{stored:?} is what the harness named the session, not a title"
            );
        }
    }

    /// The cost of the rule this replaces: a terse reference is the same
    /// string shape as a model id, so filtering one filtered the other.
    #[test]
    fn a_terse_reference_is_still_a_usable_title() {
        let sid = test_session_id();
        for prompt in ["pr-477", "issue-484", "commit-a0bc43d"] {
            let candidates = vec![obs(ObservationKind::UserPrompt, prompt)];
            assert_eq!(
                derive_title(&candidates, sid),
                prompt,
                "{prompt:?} is a plausible prompt and must survive as a title"
            );
        }
    }

    /// The title is also what `page_descriptor` filters against — a body line
    /// equal to it is dropped as a repeat. Changing which string becomes the
    /// title therefore changes which body lines survive, so a filtered page
    /// must still render a body whose prompts are intact.
    #[test]
    fn a_filtered_title_still_leaves_the_prompts_in_the_body() {
        let obs = vec![
            obs(ObservationKind::UserPrompt, "<ide_opened_file>noise"),
            obs(
                ObservationKind::UserPrompt,
                "Make the backpressure test deterministic",
            ),
        ];
        let title = derive_title(&obs, test_session_id());
        let tally = tally_session(&obs);
        let body = render_body(test_session_id(), &tally, &title);
        assert!(
            body.contains("Make the backpressure test deterministic"),
            "the promoted prompt must still appear in the body: {body}"
        );
        assert!(
            body.contains("<ide_opened_file>noise"),
            "the skipped prompt is still session history and must be recorded"
        );
    }

    #[test]
    fn title_falls_back_through_kinds() {
        let no_prompt = vec![obs(ObservationKind::PostToolUse, "Edit")];
        assert_eq!(derive_title(&no_prompt, test_session_id()), "Edit");

        let empty: Vec<Observation> = vec![];
        assert_eq!(derive_title(&empty, test_session_id()), "session");

        let with_prompt = vec![
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::UserPrompt, "fix the auth bug"),
        ];
        assert_eq!(
            derive_title(&with_prompt, test_session_id()),
            "fix the auth bug"
        );
    }

    #[test]
    fn body_includes_tool_counts_and_prompts() {
        // Each real tool call produces a Pre+Post pair. The render must
        // report one entry per call (not one per observation), so two
        // Edit calls = 2 (not 4) and one Bash call = 1 (not 2).
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "build the thing"),
            obs(ObservationKind::PreToolUse, "Edit"),
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::PreToolUse, "Edit"),
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PostToolUse, "Bash"),
            obs(ObservationKind::SessionEnd, "session"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert!(page.title.contains("build the thing"));
        assert!(page.body.contains("`Edit`: 2"));
        assert!(page.body.contains("`Bash`: 1"));
        assert!(page.body.contains("build the thing"));
    }

    /// `[ingest] record_tool_names` titles count under their tool name, a bare
    /// family label still counts under the label, and the file family they
    /// carry still marks an abrupt ending.
    #[test]
    fn recorded_tool_names_count_by_name_and_keep_the_file_signal() {
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "rework the retry loop"),
            obs(ObservationKind::PostToolUse, "tool non-file: Bash"),
            obs(ObservationKind::PostToolUse, "tool non-file: Bash"),
            obs(ObservationKind::PostToolUse, "tool unknown: Skill"),
            obs(ObservationKind::PostToolUse, "tool non-file"),
            obs(ObservationKind::PostToolUse, "tool file: Edit"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::ClaudeCode,
            &observations,
        );
        assert!(page.body.contains("- `Bash`: 2\n"), "{}", page.body);
        assert!(page.body.contains("- `Skill`: 1\n"), "{}", page.body);
        assert!(page.body.contains("- `Edit`: 1\n"), "{}", page.body);
        assert!(
            page.body.contains("- `tool non-file`: 1\n"),
            "{}",
            page.body
        );
        assert!(
            !page.body.contains("`tool non-file: Bash`"),
            "{}",
            page.body
        );
        assert!(
            page.body
                .contains("Session ended without a normal stop while working with files"),
            "{}",
            page.body
        );
    }

    /// Failed calls are counted in the summary and the newest one is quoted,
    /// with the tool name when recorded and never its arguments.
    #[test]
    fn failed_tool_calls_are_counted_and_the_last_one_is_quoted() {
        let page_for = |rows: &[Observation]| {
            synthesize_session_page(
                WorkspaceId::new(),
                ProjectId::new(),
                SessionId::new(),
                AgentKind::ClaudeCode,
                rows,
            )
        };
        let observations = vec![
            obs(ObservationKind::UserPrompt, "make the tests pass"),
            obs_with_body(
                ObservationKind::PostToolUse,
                "tool non-file: Bash",
                "tool_family: non-file\ntool_call_id: toolu_1\noutcome: error\n---\n\
                 Exit code 1\nError: Cannot find module 'express'\n    at require",
            ),
            obs_with_body(
                ObservationKind::PostToolUse,
                "tool non-file: Bash",
                "tool_family: non-file\ntool_call_id: toolu_2\noutcome: unknown\n---\n\
                 {\"stdout\":\"42 passing\"}",
            ),
        ];
        let digest = tally_session(&observations);
        assert_eq!(digest.failed_tool_calls(), 1);
        assert_eq!(
            session_summary(&digest),
            "1 prompt, 2 completed tool calls across Bash, 1 failed."
        );
        let page = page_for(&observations);
        assert!(
            page.body
                .contains("## Failed tool calls\n\n- **count:** 1\n"),
            "{}",
            page.body
        );
        assert!(
            page.body.contains(
                "- **last:** Bash: Exit code 1 Error: Cannot find module 'express' at require\n"
            ),
            "{}",
            page.body
        );
        assert!(!page.body.contains("toolu_1"), "{}", page.body);

        // A family label names no tool; a header-only body still counts.
        let bare = vec![
            obs_with_body(
                ObservationKind::PostToolUse,
                "tool file",
                "tool_family: file\noutcome: error\n---\nENOENT: no such file",
            ),
            obs_with_body(
                ObservationKind::PostToolUse,
                "tool unknown",
                "tool_family: unknown\nmcp_server: aim\nmcp_tool: memory_query\noutcome: error",
            ),
        ];
        let digest = tally_session(&bare);
        assert_eq!(digest.failed_tool_calls(), 2);
        assert_eq!(
            digest
                .handoff_facts()
                .last_failed_tool
                .as_ref()
                .map(FailedToolFact::describe),
            Some("(no error text captured)".to_string())
        );
        assert_eq!(
            session_summary(&digest),
            "2 completed tool calls, 2 failed."
        );
        let page = page_for(&bare);
        assert!(
            page.body.contains("- **last:** (no error text captured)\n"),
            "{}",
            page.body
        );

        // Control: no failures, nothing added.
        let fine = vec![obs_with_body(
            ObservationKind::PostToolUse,
            "tool non-file: Bash",
            "tool_family: non-file\noutcome: unknown\n---\nok",
        )];
        let digest = tally_session(&fine);
        assert_eq!(digest.failed_tool_calls(), 0);
        assert!(digest.handoff_facts().last_failed_tool.is_none());
        assert_eq!(
            session_summary(&digest),
            "1 completed tool call across Bash."
        );
        let page = page_for(&fine);
        assert!(!page.body.contains("Failed tool calls"), "{}", page.body);
    }

    /// Adversarial: the marker is read from the header only, so a tool output
    /// that echoes a stored body is not a failure. A long error is capped.
    #[test]
    fn an_outcome_marker_inside_tool_output_is_not_a_failure() {
        let echoed = obs_with_body(
            ObservationKind::PostToolUse,
            "tool non-file: Bash",
            "tool_family: non-file\noutcome: unknown\n---\n\
             tool_family: file\noutcome: error\n---\nquoted from a page\noutcome: error",
        );
        let prompt = obs_with_body(
            ObservationKind::UserPrompt,
            "why outcome: error",
            "tool_family: file\noutcome: error\n---\nnot a tool row",
        );
        let digest = tally_session(&[prompt, echoed]);
        assert_eq!(digest.failed_tool_calls(), 0);
        assert!(digest.handoff_facts().last_failed_tool.is_none());

        let long = obs_with_body(
            ObservationKind::PostToolUse,
            "Bash",
            &format!(
                "tool_family: non-file\noutcome: error\n---\n{}",
                "e ".repeat(400)
            ),
        );
        let fact = failed_tool_fact(&long).expect("a failed call");
        assert!(
            fact.error.len() <= FAILED_TOOL_EXCERPT_BYTES,
            "{}",
            fact.error
        );
        assert!(fact.error.ends_with('…'));
        assert_eq!(fact.describe(), format!("Bash: {}", fact.error));
    }

    /// The store and the router spell the marker in different crates; keep
    /// them equal.
    #[test]
    fn the_failed_outcome_marker_is_the_one_the_router_writes() {
        assert_eq!(
            format!(
                "{}{}",
                ai_memory_core::TOOL_OUTCOME_LINE_PREFIX,
                crate::capture_policy::ToolOutcome::Error.as_str()
            ),
            FAILED_TOOL_OUTCOME_LINE
        );
    }

    #[test]
    fn pre_only_tool_calls_are_not_counted() {
        // A PreToolUse without a matching PostToolUse (cancelled / crashed
        // mid-call) intentionally drops out of the count rather than
        // inflating it.
        let observations = vec![
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PostToolUse, "Bash"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert!(page.body.contains("`Bash`: 1"));
    }

    #[test]
    fn body_includes_opt_in_extension_source_event() {
        let mut custom = obs(ObservationKind::Other, "Lead contacted");
        custom.extension = Some("fstech".into());
        custom.source_event = Some("lead.contact".into());

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &[custom],
        );

        assert!(page.body.contains("`other [fstech:lead.contact]`"));
    }

    /// #1221: the zero-LLM page must say when a session ended abruptly,
    /// reusing the `derive_open_questions` heuristics. A final prompt that
    /// is a question, with no Stop after it, means the prompt was never
    /// answered.
    #[test]
    fn an_unanswered_final_prompt_flags_the_abrupt_ending() {
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "why is the cache empty?"),
            obs(ObservationKind::PostToolUse, "tool non-file"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert!(
            page.body.contains(
                "Session ended without a normal stop; the final prompt was never answered."
            ),
            "the abrupt-ending line must be in the body: {}",
            page.body
        );
    }

    /// File activity with no subsequent Stop: the session ended while
    /// working in the tree, so the page must say the changes may be
    /// unverified.
    #[test]
    fn file_activity_without_a_stop_flags_the_abrupt_ending() {
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "rework the retry loop"),
            obs(ObservationKind::PostToolUse, "tool file"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert!(
            page.body
                .contains("Session ended without a normal stop while working with files"),
            "the file-activity line must be in the body: {}",
            page.body
        );
    }

    /// A Stop with no SessionEnd is a mid-task exit.
    #[test]
    fn a_stop_without_session_end_flags_the_mid_task_exit() {
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "rework the retry loop"),
            obs(ObservationKind::Stop, "stop"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert!(
            page.body.contains("may have exited mid-task"),
            "the mid-task line must be in the body: {}",
            page.body
        );
    }

    /// Control: a clean SessionStart → prompt → tool → Stop → SessionEnd
    /// session gets no terminal-state line at all.
    #[test]
    fn a_cleanly_closed_session_has_no_abrupt_ending_line() {
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "rework the retry loop"),
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::Stop, "stop"),
            obs(ObservationKind::SessionEnd, "session-end"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );
        assert!(
            !page.body.contains("Session ended without a normal stop"),
            "a normal close must not be flagged: {}",
            page.body
        );
        assert!(
            !page.body.contains("may have exited mid-task"),
            "a normal close must not be flagged: {}",
            page.body
        );
    }

    #[test]
    fn raw_observations_small_session_includes_all_entries() {
        let observations: Vec<Observation> = (0..5)
            .map(|i| obs(ObservationKind::Other, &format!("entry-{i}")))
            .collect();

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );

        for i in 0..5 {
            assert!(page.body.contains(&format!("entry-{i}")));
        }
        assert!(!page.body.contains("raw observations omitted"));
    }

    #[test]
    fn raw_observations_large_session_omits_middle_with_count() {
        let observations: Vec<Observation> = (0..600)
            .map(|i| obs(ObservationKind::Other, &format!("entry-{i}")))
            .collect();

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            AgentKind::Codex,
            &observations,
        );

        assert!(page.body.contains("entry-0"));
        assert!(page.body.contains("entry-249"));
        assert!(!page.body.contains("entry-250"));
        assert!(!page.body.contains("entry-349"));
        assert!(page.body.contains("entry-350"));
        assert!(page.body.contains("entry-599"));
        assert!(
            page.body
                .contains("100 raw observations omitted from the middle")
        );
    }
}

#[cfg(test)]
mod load_with_count_scopes {
    use ai_memory_core::{
        AgentKind, NewObservation, NewSession, ObservationKind, Sanitized, Sanitizer,
    };
    use ai_memory_store::Store;

    use super::*;

    /// The new raw-id entry point must refuse foreign tuples the way the
    /// bounded read refuses them: the idle tick's digest can never load a
    /// session's observations through a scope the session does not belong
    /// to. Each refusal is paired with the legitimate control in the same
    /// fixture.
    #[tokio::test]
    async fn foreign_and_missing_tuples_load_nothing_the_right_tuple_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "idle", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        let foreign_ws = store
            .writer
            .get_or_create_workspace("foreign".to_string())
            .await
            .unwrap();
        let sid = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
                occurred_at: None,
            })
            .await
            .unwrap();
        async fn insert_obs(
            store: &Store,
            sid: SessionId,
            ws: WorkspaceId,
            proj: ProjectId,
            kind: ObservationKind,
            body: &str,
        ) {
            let obs = NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: proj,
                kind,
                extension: None,
                source_event: None,
                title: "scoped test event".into(),
                body: body.into(),
                importance: 5,
                occurred_at: None,
            };
            store
                .writer
                .insert_observation(Sanitized::new(obs, &Sanitizer::builtin()))
                .await
                .unwrap();
        }
        insert_obs(&store, sid, ws, proj, ObservationKind::UserPrompt, "prompt").await;
        insert_obs(&store, sid, ws, proj, ObservationKind::PostToolUse, "tool").await;
        let reader = store.reader.clone();

        // Violations: a foreign project, a foreign workspace, a missing id —
        // each is refused, so none of them can load the session's rows.
        let foreign_project = SessionDigest::load_with_count(&reader, ws, other, sid).await;
        assert!(
            matches!(foreign_project, Err(StoreError::SessionCollision)),
            "a foreign project loads no observations"
        );
        let foreign_workspace =
            SessionDigest::load_with_count(&reader, foreign_ws, proj, sid).await;
        assert!(
            matches!(foreign_workspace, Err(StoreError::SessionCollision)),
            "a foreign workspace loads no observations"
        );
        let missing = SessionDigest::load_with_count(&reader, ws, proj, SessionId::new()).await;
        assert!(
            matches!(missing, Err(StoreError::SessionCollision)),
            "a missing id loads nothing"
        );

        // Legitimate control: the right tuple loads the session's rows and
        // counts them from the same snapshot.
        let (digest, count) = SessionDigest::load_with_count(&reader, ws, proj, sid)
            .await
            .unwrap();
        assert_eq!(count, 2, "the digest's snapshot counts both rows");
        assert_eq!(digest.total, 2);
    }
}
