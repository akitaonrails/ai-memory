//! The bounded SessionEnd reads against the full row load they replaced.
//!
//! A SessionEnd used to load every observation of the session and render the
//! page and the handoff from that list. It now reads a [`SessionDigest`] with
//! bounded queries. The claim is that nothing the page, the handoff or the
//! lifecycle-only decision contain changed, so this file keeps the original
//! code as the oracle: `full_load` below is `synth.rs` and the handoff half of
//! `router.rs` at the commit before the change, byte for byte except for the
//! two `pub(super)` that let the tests call it and the imports it needs from
//! its new home. It exists in test code only.
//!
//! Each test seeds sessions into a real store, runs the oracle on the rows
//! `observations_for_session` returns, runs the production path on the same
//! session, and compares the outputs.
//!
//! The failed-call facts (`## Failed tool calls`, the `N failed` clause, the
//! handoff's `Last failed tool call` line) post-date the oracle. No fixture
//! here has a failed call; `failed_tool_facts_match_the_row_list_and_stay_bounded`
//! checks them against the row-list digest instead.

#[rustfmt::skip]
mod full_load {
    use std::collections::BTreeMap;

    use ai_memory_core::{
        AgentKind, NewHandoff, NewPage, Observation, ObservationKind, PagePath, ProjectId,
        SessionId, Tier, WorkspaceId, looks_like_markup_block, looks_like_scaffolding,
        truncate_for_title,
    };
    use jiff::tz::TimeZone;

    use crate::capture_policy::ToolFamily;
    use crate::payload::is_safe_tool_title;
    use crate::router::{cap_handoff_text, is_acknowledgment};

    const RAW_OBSERVATION_MAX_LINES: usize = 500;
    const RAW_OBSERVATION_HEAD_LINES: usize = 250;
    const RAW_OBSERVATION_TAIL_LINES: usize =
        RAW_OBSERVATION_MAX_LINES - RAW_OBSERVATION_HEAD_LINES;
    const SUBAGENT_PROMPT_PREAMBLE: &str = "You are a subagent spawned by another session.";

/// Build a [`NewPage`] from the observations collected during a session.
///
/// The returned page is *always* under `sessions/<session-id>.md` so each
/// session has a stable URL the user can bookmark.
#[must_use]
pub(super) fn synthesize_session_page(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    agent_kind: AgentKind,
    observations: &[Observation],
) -> NewPage {
    // One tally for the whole page: the body renderer and the summary builder
    // describe the same session and must not scan it twice to do so.
    let tally = tally_session(observations);
    let title = derive_title(observations, session_id);
    let body = render_body(session_id, observations, &title, &tally);
    let mut frontmatter_json = serde_json::json!({
        "title": title,
        "session_id": session_id.to_string(),
        // Origin, not writer: callers pass the immutable value persisted on
        // the session row, so checkpoints and later superseding versions keep
        // naming the harness that produced the session.
        "agent": agent_kind.as_str(),
        "tier": "episodic",
    });
    let summary = session_summary(&tally);
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
fn derive_title(observations: &[Observation], session_id: SessionId) -> String {
    let mut rejected_subagent_preamble = false;
    for obs in observations {
        if obs.kind != ObservationKind::UserPrompt {
            continue;
        }
        if obs.title.trim() == SUBAGENT_PROMPT_PREAMBLE {
            rejected_subagent_preamble = true;
            if let Some(title) = obs
                .body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !looks_like_title_scaffolding(line))
            {
                return truncate_for_title(title);
            }
            continue;
        }
        if !obs.title.is_empty() && !looks_like_scaffolding(&obs.title) {
            return obs.title.clone();
        }
    }
    if rejected_subagent_preamble {
        return format!("Session {session_id}");
    }
    for obs in observations {
        // The router titles an untitled lifecycle event with its kind name
        // (`stop`, `session-end`): a string every such page would share, and
        // what this loop reached once tool-family labels were skipped (#895).
        if obs.kind == ObservationKind::SessionStart || obs.title == obs.kind.as_str() {
            continue;
        }
        // A `tool <family>` label is a partition of the calls, not a title:
        // `looks_like_scaffolding` does not catch it, so reject it here or a
        // session whose only non-prompt observation is a closed-tool call
        // ("tool non-file") would take the label as its page title.
        if !obs.title.is_empty()
            && !looks_like_title_scaffolding(&obs.title)
            && !is_safe_tool_title(&obs.title)
        {
            return obs.title.clone();
        }
    }
    // Every candidate was scaffolding. The page still needs a title, and its
    // own identity is more use to a reader than the word "session" repeated
    // across every such page.
    if observations.is_empty() {
        return "session".to_string();
    }
    format!("Session {session_id}")
}

fn looks_like_title_scaffolding(candidate: &str) -> bool {
    candidate.trim() == SUBAGENT_PROMPT_PREAMBLE || looks_like_scaffolding(candidate)
}

/// The per-session counts that both the rendered body and the frontmatter
/// summary are built from.
struct SessionTally<'a> {
    /// Completed calls per tool name, keyed for stable ordering.
    tool_counts: BTreeMap<&'a str, usize>,
    /// User prompts in arrival order.
    prompts: Vec<&'a Observation>,
    /// First `SessionStart`, if the session recorded one.
    start: Option<&'a Observation>,
    /// Last `SessionEnd`, if the session recorded one.
    end: Option<&'a Observation>,
}

/// Tally a session's observations once.
///
/// [`render_body`] and [`session_summary`] both need these counts, and
/// [`synthesize_session_page`] computes them once and lends them to both — so
/// a page costs one pass over its observations, not two, and the body and the
/// summary cannot describe the same session differently. It also keeps the
/// summary from having to parse the counts back out of the markdown the body
/// just rendered them into.
fn tally_session(observations: &[Observation]) -> SessionTally<'_> {
    let mut tally = SessionTally {
        tool_counts: BTreeMap::new(),
        prompts: Vec::new(),
        start: None,
        end: None,
    };
    for obs in observations {
        match obs.kind {
            ObservationKind::SessionStart => tally.start = Some(obs),
            ObservationKind::SessionEnd => tally.end = Some(obs),
            ObservationKind::UserPrompt => tally.prompts.push(obs),
            // Count only PostToolUse — each tool call produces both a
            // PreToolUse and a PostToolUse observation, so counting both
            // doubles every reported number ("Bash: 4" for two real calls).
            // PostToolUse is the "completed call" event; pre-only calls
            // that never produced a post (cancellations) are intentionally
            // excluded.
            ObservationKind::PostToolUse if !obs.title.is_empty() => {
                *tally.tool_counts.entry(obs.title.as_str()).or_insert(0) += 1;
            }
            _ => {}
        }
    }
    tally
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
fn session_summary(tally: &SessionTally<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !tally.prompts.is_empty() {
        parts.push(format!(
            "{} prompt{}",
            tally.prompts.len(),
            plural(tally.prompts.len())
        ));
    }

    let calls: usize = tally.tool_counts.values().sum();
    if calls > 0 {
        // Name the busiest tools first; ties fall back to the map's
        // alphabetical order so the same session always renders the same way.
        let mut by_calls: Vec<(&str, usize)> =
            tally.tool_counts.iter().map(|(k, v)| (*k, *v)).collect();
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

    if let (Some(start), Some(end)) = (tally.start, tally.end)
        && let Some(spent) =
            format_duration(end.created_at.as_second() - start.created_at.as_second())
    {
        parts.push(format!("over {spent}"));
    }

    // The harness's own word for why the session ended (#1225), when the
    // SessionEnd observation carried one. Display-bounded: a summary line
    // is no place for a long free-text field.
    if let Some(reason) = tally
        .end
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

fn render_body(
    session_id: SessionId,
    observations: &[Observation],
    title: &str,
    tally: &SessionTally<'_>,
) -> String {
    let SessionTally {
        tool_counts,
        prompts,
        start,
        end,
    } = tally;

    let mut buf = String::with_capacity(2048);
    buf.push_str(&format!("# {title}\n\n"));

    buf.push_str("## Session metadata\n\n");
    buf.push_str(&format!("- **session_id:** `{session_id}`\n"));
    if let Some(s) = start {
        buf.push_str(&format!("- **started_at:** {}\n", human_ts(&s.created_at),));
    }
    if let Some(e) = end {
        buf.push_str(&format!("- **ended_at:** {}\n", human_ts(&e.created_at),));
    }
    buf.push_str(&format!("- **observations:** {}\n\n", observations.len()));

    if let Some(note) = abrupt_ending_note(observations) {
        buf.push_str(&format!("{note}\n\n"));
    }

    if !prompts.is_empty() {
        buf.push_str("## Prompts\n\n");
        for (i, p) in prompts.iter().enumerate() {
            buf.push_str(&format!("{}. {}\n", i + 1, p.title));
        }
        buf.push('\n');
    }

    if !tool_counts.is_empty() {
        buf.push_str("## Tool calls\n\n");
        for (name, count) in tool_counts {
            buf.push_str(&format!("- `{name}`: {count}\n"));
        }
        buf.push('\n');
    }

    buf.push_str("## Raw observations\n\n");
    render_raw_observations(&mut buf, observations);

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
fn abrupt_ending_note(observations: &[Observation]) -> Option<&'static str> {
    let last_of_kind = |kind: ObservationKind| observations.iter().rev().find(|o| o.kind == kind);
    let last_stop = last_of_kind(ObservationKind::Stop);
    let has_session_end = observations
        .iter()
        .any(|o| o.kind == ObservationKind::SessionEnd);
    // Harness blocks delivered as a user turn are not the user's question.
    let last_prompt = observations.iter().rev().find(|o| {
        let text = if o.body.is_empty() { &o.title } else { &o.body };
        o.kind == ObservationKind::UserPrompt && !looks_like_markup_block(text)
    });

    if last_stop.is_none()
        && let Some(prompt) = last_prompt
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

    if last_stop.is_none()
        && let Some(tool) = last_of_kind(ObservationKind::PostToolUse)
        && crate::payload::tool_family_from_title(&tool.title) == Some(ToolFamily::File)
    {
        return Some(
            "Session ended without a normal stop while working with files; \
             the working tree may hold uncommitted or unverified changes.",
        );
    }

    if last_stop.is_some() && !has_session_end {
        return Some(
            "A stop was recorded but no session end; the session may have exited mid-task.",
        );
    }

    None
}

fn render_raw_observations(buf: &mut String, observations: &[Observation]) {
    if observations.len() <= RAW_OBSERVATION_MAX_LINES {
        for obs in observations {
            render_raw_observation(buf, obs);
        }
        return;
    }

    for obs in &observations[..RAW_OBSERVATION_HEAD_LINES] {
        render_raw_observation(buf, obs);
    }
    let omitted = observations.len() - RAW_OBSERVATION_HEAD_LINES - RAW_OBSERVATION_TAIL_LINES;
    buf.push_str(&format!(
        "\n_... {omitted} raw observations omitted from the middle (showing first {RAW_OBSERVATION_HEAD_LINES} and last {RAW_OBSERVATION_TAIL_LINES})._\n\n",
    ));
    for obs in &observations[observations.len() - RAW_OBSERVATION_TAIL_LINES..] {
        render_raw_observation(buf, obs);
    }
}

fn render_raw_observation(buf: &mut String, obs: &Observation) {
    let kind = observation_kind_label(obs);
    buf.push_str(&format!(
        "- `{}` @ {} — {}\n",
        kind,
        human_ts(&obs.created_at),
        obs.title.chars().take(80).collect::<String>(),
    ));
}

fn observation_kind_label(obs: &Observation) -> String {
    match (&obs.extension, &obs.source_event) {
        (Some(extension), Some(source_event)) => {
            format!("{} [{}:{}]", obs.kind.as_str(), extension, source_event)
        }
        _ => obs.kind.as_str().to_string(),
    }
}

fn human_ts(ts: &jiff::Timestamp) -> String {
    ts.to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_auto_handoff(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    from_agent: AgentKind,
    session_id: SessionId,
    cwd: Option<String>,
    observations: &[ai_memory_core::Observation],
    owner_user: Option<String>,
    turn_checkpoint: bool,
) -> NewHandoff {
    // Prefer obs.body (the full prompt) over obs.title (first-line +
    // truncated to 80 chars for log/list display). When body is
    // empty fall back to title so we never produce an empty entry.
    fn pick_text(obs: &ai_memory_core::Observation) -> &str {
        if !obs.body.is_empty() {
            obs.body.as_str()
        } else {
            obs.title.as_str()
        }
    }
    /// Cap so a single 10-page prompt doesn't blow up the handoff.
    /// The body is already scrubbed at insert time; this is just a
    /// length budget. 1500 chars ≈ 250 words ≈ a paragraph.
    fn cap(s: &str) -> String {
        const MAX: usize = 1500;
        if s.chars().count() <= MAX {
            s.to_string()
        } else {
            let truncated: String = s.chars().take(MAX).collect();
            format!("{truncated}…")
        }
    }
    let mut prompts: Vec<String> = Vec::new();
    let mut tools: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for obs in observations {
        match obs.kind {
            ObservationKind::UserPrompt => {
                let text = pick_text(obs);
                // A harness block delivered as a user turn (`<task-notification>`)
                // is not a request to continue from.
                if !text.is_empty() && !ai_memory_core::looks_like_markup_block(text) {
                    prompts.push(text.to_string());
                }
            }
            // Skip `tool <family>` / bare-`<family>` labels: a family is a
            // partition of the calls, not a name for a tool, so "Tools used:
            // tool file, tool non-file" tells the receiver nothing. Only a
            // harness's own tool name is worth listing.
            ObservationKind::PostToolUse | ObservationKind::PreToolUse
                if !obs.title.is_empty()
                    && crate::payload::tool_family_from_title(&obs.title).is_none() =>
            {
                tools.insert(obs.title.as_str());
            }
            _ => {}
        }
    }
    let first_prompt = prompts.first().cloned();
    let last_prompt = prompts.last().cloned();
    let open_questions = if turn_checkpoint {
        // A completed turn is neither a native-session exit nor evidence that
        // the user's last question remains unanswered.
        last_prompt
            .as_deref()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty() && !is_acknowledgment(prompt))
            .map(|prompt| format!("Continue from last request: {}", cap_handoff_text(prompt)))
            .into_iter()
            .collect()
    } else {
        derive_open_questions(observations, &last_prompt)
    };
    // A heuristic that quotes the last prompt already carries it; the summary
    // must not repeat it.
    let last_quoted = last_prompt.as_deref().is_some_and(|last| {
        let quoted = cap_handoff_text(last.trim());
        !quoted.is_empty() && open_questions.iter().any(|q| q.ends_with(quoted.as_str()))
    });
    let mut summary = match (&first_prompt, &last_prompt) {
        (Some(first), Some(last)) if first == last && last_quoted => {
            "Single-prompt session; the prompt is quoted under open questions.".to_string()
        }
        (Some(first), Some(last)) if first == last => format!("Session focused on: {}", cap(first)),
        (Some(first), Some(_)) if last_quoted => format!("Started: {}", cap(first)),
        (Some(first), Some(last)) => format!("Started: {}\n\nLast: {}", cap(first), cap(last),),
        (Some(first), None) => format!("Started: {}", cap(first)),
        _ => format!(
            "{}; {} observations recorded.",
            if turn_checkpoint {
                "Turn checkpoint"
            } else {
                "Session ended"
            },
            observations.len()
        ),
    };
    // Stop bodies exist only after the assistant-capture double opt-in. The
    // excerpt continues the work in the next session's baton and stays out of
    // the git-tracked session page.
    if let Some(assistant) = observations.iter().rev().find(|observation| {
        observation.kind == ObservationKind::Stop && !observation.body.trim().is_empty()
    }) {
        summary.push_str("\n\nLatest assistant response: ");
        summary.push_str(&assistant.body);
    }
    let next_steps = if tools.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Tools used: {}",
            tools.into_iter().collect::<Vec<_>>().join(", ")
        )]
    };
    NewHandoff {
        workspace_id,
        project_id,
        from_session_id: Some(session_id),
        from_agent,
        to_agent: None,
        cwd: cwd.map(std::path::PathBuf::from),
        summary,
        open_questions,
        next_steps,
        files_touched: Vec::new(),
        owner_user,
    }
}

/// Derive the `open_questions` field for an automatic SessionEnd handoff
/// using multi-signal heuristics, instead of blindly copying the last user
/// prompt.
///
/// The previous implementation always did `"Continue from: <last prompt>"`,
/// which produced useless entries when the last prompt was an acknowledgment
/// ("ok", "thanks", "好的") or when the session ended mid-task after an edit
/// but before verification. This function inspects the *tail* of the
/// observation stream to choose the most informative framing.
///
/// Heuristics (first match wins):
/// 1. Last prompt ends with `?` or `？` → tag as an unresolved question.
/// 2. Last tool was an edit/write/patch with no subsequent Stop → the
///    session ended before verification; advise the receiver to run tests.
/// 3. Has Stop but no SessionEnd → mid-task exit; use the last
///    *substantive* prompt (filtering acknowledgments).
/// 4. Normal end → last substantive prompt as "Continue from: …".
/// 5. Fallback → empty Vec (same as the old behaviour when no prompts).
fn derive_open_questions(
    observations: &[ai_memory_core::Observation],
    last_prompt: &Option<String>,
) -> Vec<String> {
    let last_of_kind = |kind: ObservationKind| -> Option<&ai_memory_core::Observation> {
        observations.iter().rev().find(|o| o.kind == kind)
    };

    // Harness blocks delivered as a user turn are not the user's question.
    let last_prompt_obs = observations.iter().rev().find(|o| {
        let text = if o.body.is_empty() { &o.title } else { &o.body };
        o.kind == ObservationKind::UserPrompt && !ai_memory_core::looks_like_markup_block(text)
    });
    let last_tool = last_of_kind(ObservationKind::PostToolUse);
    let last_stop = last_of_kind(ObservationKind::Stop);
    let has_session_end = observations
        .iter()
        .any(|o| o.kind == ObservationKind::SessionEnd);

    // Heuristic 1: last prompt is a question → it is the open question.
    if let Some(p) = last_prompt_obs {
        let body = p.body.trim();
        if body.ends_with('?') || body.ends_with('？') {
            return vec![format!("Unresolved question: {}", cap_handoff_text(body))];
        }
    }

    // Heuristic 2: file activity with no subsequent Stop → the session ended
    // abnormally while working in the tree.
    //
    // The signal is the tool *family*, not the tool name. A PostToolUse
    // observation's title carries the family in one of two spellings: the
    // majority path (every closed-tool agent) writes `safe_tool_title`'s
    // `"tool file"` / `"tool non-file"` / …, while the reserved-protocol
    // path writes the bare `canonical_tool_name` form `"file"` / …. Both
    // must match here, and `tool_family_from_title` recognises both; a raw
    // tool name ("edit"/"write"/"patch") is neither and correctly does not.
    //
    // That same closed schema means read and write are indistinguishable:
    // `ToolFamily::File` covers both, and `ToolOutcome` is only
    // success/error/unknown. So this cannot honestly claim changes were
    // made — only that the session touched files and then ended without a
    // normal Stop, which is worth telling the receiver either way.
    if let Some(tool) = last_tool {
        let touched_files =
            crate::payload::tool_family_from_title(&tool.title) == Some(ToolFamily::File);
        if touched_files && last_stop.is_none() {
            return vec![
                "Session ended without a normal stop while working with files".into(),
                "Check the working tree for uncommitted or unverified changes".into(),
            ];
        }
    }

    // Heuristic 3 & 4: use the last *substantive* prompt.
    if let Some(p) = last_prompt {
        let body = p.trim();
        if !body.is_empty() && !is_acknowledgment(body) {
            // Heuristic 3 signal: no SessionEnd → flag as mid-task.
            if !has_session_end && last_stop.is_some() {
                return vec![format!(
                    "Continue from (mid-task exit): {}",
                    cap_handoff_text(body)
                )];
            }
            // Heuristic 4: normal end.
            return vec![format!("Continue from: {}", cap_handoff_text(body))];
        }
    }

    // Fallback: no substantive prompt → empty (same as old behaviour).
    Vec::new()
}
}

use ai_memory_core::{
    AgentKind, NewSession, ObservationId, ObservationKind, ProjectId, SessionId, WorkspaceId,
};
use ai_memory_store::Store;
use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::{HandoffFacts, SessionDigest, is_ephemeral_session};

/// `created_at` of the first row of every fixture, in microseconds.
const BASE_US: i64 = 1_780_000_000_000_000;
/// The longest raw-observation list a page prints whole.
const RAW_CAP: usize = 500;
/// Slack the bound tests allow on top of the head and tail lines: one row per
/// early-exit scan (end marker, first and last prompt, question prompt, last
/// tool, latest assistant response).
const SCAN_SLACK: usize = 16;
const SUBAGENT_PREAMBLE: &str = "You are a subagent spawned by another session.";

/// A harness block delivered as a user turn: not a request to continue from.
const TASK_NOTIFICATION: &str =
    "<task-notification>\n<task-id>abc</task-id>\n<status>completed</status>\n</task-notification>";

/// One observation row to seed, with its time in whole seconds after
/// [`BASE_US`] (equal seconds are ties, ordered by insertion).
#[derive(Clone)]
struct Row {
    kind: ObservationKind,
    title: String,
    body: String,
    at: i64,
    extension: Option<&'static str>,
    source_event: Option<&'static str>,
}

fn row(kind: ObservationKind, title: &str, body: &str, at: i64) -> Row {
    Row {
        kind,
        title: title.to_string(),
        body: body.to_string(),
        at,
        extension: None,
        source_event: None,
    }
}

struct Fixture {
    _tmp: TempDir,
    store: Store,
    ws: WorkspaceId,
    proj: ProjectId,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "equivalence", None)
        .await
        .unwrap();
    Fixture {
        _tmp: tmp,
        store,
        ws,
        proj,
    }
}

impl Fixture {
    /// Seed one session with `rows` in order, with plain SQL in one
    /// transaction: tens of thousands of rows through the writer actor would
    /// spend the test's whole time budget on the fixture.
    async fn seed(&self, rows: &[Row]) -> SessionId {
        let sid = SessionId::new();
        self.store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: sid,
                workspace_id: self.ws,
                project_id: self.proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        let mut conn = Connection::open(self.store.db_path()).unwrap();
        conn.pragma_update(None, "synchronous", "OFF").unwrap();
        // The FTS trigger indexes every inserted row, which is most of what
        // seeding thousands of rows costs; nothing here searches them.
        conn.execute_batch("DROP TRIGGER IF EXISTS observations_fts_ai")
            .unwrap();
        let tx = conn.transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO observations \
                     (id, session_id, workspace_id, project_id, kind, extension, source_event, \
                      title, body, importance, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 5, ?10)",
                )
                .unwrap();
            for r in rows {
                insert
                    .execute(params![
                        ObservationId::new().as_bytes(),
                        sid.as_bytes(),
                        self.ws.as_bytes(),
                        self.proj.as_bytes(),
                        r.kind.as_str(),
                        r.extension,
                        r.source_event,
                        r.title,
                        r.body,
                        BASE_US + r.at * 1_000_000,
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        sid
    }

    /// Everything the SessionEnd path renders, through the bounded reads and
    /// through the row load they replaced, and the claim that they agree.
    async fn assert_equivalent(&self, name: &str, sid: SessionId) {
        let reader = &self.store.reader;
        // The original path: every row, then the original row-list code.
        let rows = reader.observations_for_session(sid).await.unwrap();
        let agent = AgentKind::ClaudeCode;
        let old_page = full_load::synthesize_session_page(self.ws, self.proj, sid, agent, &rows);

        let digest = SessionDigest::load(reader, self.ws, self.proj, sid)
            .await
            .unwrap();
        let new_page = super::synthesize_session_page(self.ws, self.proj, sid, agent, &digest);
        assert_eq!(new_page.path, old_page.path, "{name}: page path");
        assert_eq!(new_page.title, old_page.title, "{name}: page title");
        assert_eq!(
            new_page.frontmatter_json, old_page.frontmatter_json,
            "{name}: page frontmatter"
        );
        // The oracle predates the prompt-list cap: fold its full prompt list
        // down to the capped form the new path prints before comparing, so a
        // session over the cap still compares byte for byte everywhere else.
        let prompt_titles: Vec<String> = rows
            .iter()
            .filter(|r| r.kind == ObservationKind::UserPrompt)
            .map(|r| r.title.clone())
            .collect();
        assert_eq!(
            new_page.body,
            prompt_capped_body(&old_page.body, &prompt_titles),
            "{name}: page body"
        );

        for turn_checkpoint in [false, true] {
            let old_handoff = full_load::build_auto_handoff(
                self.ws,
                self.proj,
                agent,
                sid,
                Some("/work/repo".into()),
                &rows,
                Some("owner".into()),
                turn_checkpoint,
            );
            let new_handoff = super::build_auto_handoff(
                self.ws,
                self.proj,
                agent,
                sid,
                Some("/work/repo".into()),
                &digest,
                Some("owner".into()),
                turn_checkpoint,
            );
            let ctx = format!("{name}: handoff (turn_checkpoint={turn_checkpoint})");
            assert_eq!(new_handoff.summary, old_handoff.summary, "{ctx} summary");
            assert_eq!(
                new_handoff.open_questions, old_handoff.open_questions,
                "{ctx} open_questions"
            );
            assert_eq!(
                new_handoff.next_steps, old_handoff.next_steps,
                "{ctx} next_steps"
            );
            assert_eq!(
                new_handoff.files_touched, old_handoff.files_touched,
                "{ctx} files_touched"
            );
            assert_eq!(new_handoff.cwd, old_handoff.cwd, "{ctx} cwd");
            assert_eq!(
                new_handoff.owner_user, old_handoff.owner_user,
                "{ctx} owner"
            );
            assert_eq!(
                new_handoff.from_session_id, old_handoff.from_session_id,
                "{ctx} session"
            );
        }

        // The lifecycle-only decision: one existence query, same verdict as
        // the original in-memory predicate.
        let ephemeral = is_ephemeral_session(&rows);
        assert_eq!(
            !reader
                .session_has_substantive_observation(self.ws, self.proj, sid)
                .await
                .unwrap(),
            ephemeral,
            "{name}: ephemeral decision (existence query)"
        );
        assert_eq!(
            digest.is_substantive(),
            !ephemeral,
            "{name}: ephemeral decision (digest)"
        );

        // And the digest itself is the one walking the rows gives, which is
        // what the unit tests of the renderers are written against.
        assert_eq!(
            digest,
            SessionDigest::from_observations(sid, &rows),
            "{name}: database digest vs row-list digest"
        );
        assert_eq!(
            digest.handoff_facts(),
            &HandoffFacts::from_observations(&rows),
            "{name}: handoff facts"
        );
    }
}

/// The oracle page predates the prompt-list cap: it lists every prompt.
/// Replace that section with the capped form the new path prints — the first
/// and last slice, one omission line between them, each title numbered with
/// its real position in the session — so the rest of the page still compares
/// byte for byte. Sessions with at most `PROMPT_TITLES_MAX` prompts come
/// back untouched, which is what keeps the small-session tests exact.
fn prompt_capped_body(body: &str, titles: &[String]) -> String {
    use crate::synth::{PROMPT_TITLES_HEAD, PROMPT_TITLES_MAX, PROMPT_TITLES_TAIL};
    if titles.len() <= PROMPT_TITLES_MAX {
        return body.to_string();
    }
    let heading = "## Prompts\n\n";
    let start = body
        .find(heading)
        .expect("the oracle page lists its prompts");
    // Rebuild the old section from `titles` instead of slicing the body, so
    // a title that looks like a heading cannot move the boundary.
    let old_section: String = titles
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {t}\n", i + 1))
        .collect();
    let end = start + heading.len() + old_section.len();
    debug_assert!(
        body[end..].starts_with('\n'),
        "the section runs to the next heading"
    );
    let (head, tail) = (
        &titles[..PROMPT_TITLES_HEAD],
        &titles[titles.len() - PROMPT_TITLES_TAIL..],
    );
    let mut capped = String::new();
    for (i, t) in head.iter().enumerate() {
        capped.push_str(&format!("{}. {t}\n", i + 1));
    }
    capped.push_str(&format!(
        "\n_... {} prompts omitted from the middle (showing first {PROMPT_TITLES_HEAD} and last {PROMPT_TITLES_TAIL})._\n\n",
        titles.len() - PROMPT_TITLES_MAX
    ));
    for (i, t) in tail.iter().enumerate() {
        capped.push_str(&format!(
            "{}. {t}\n",
            titles.len() - PROMPT_TITLES_TAIL + i + 1
        ));
    }
    let mut out = String::with_capacity(body.len());
    out.push_str(&body[..start]);
    out.push_str(heading);
    out.push_str(&capped);
    out.push_str(&body[end..]);
    out
}

fn start(at: i64) -> Row {
    row(ObservationKind::SessionStart, "session-start", "", at)
}

fn prompt(text: &str, at: i64) -> Row {
    row(ObservationKind::UserPrompt, text, text, at)
}

fn end(at: i64) -> Row {
    row(
        ObservationKind::SessionEnd,
        "session-end",
        "reason: logout",
        at,
    )
}

/// A pre/post pair of one tool.
fn tool_pair(title: &str, at: i64) -> [Row; 2] {
    [
        row(ObservationKind::PreToolUse, title, "", at),
        row(ObservationKind::PostToolUse, title, "", at + 1),
    ]
}

// ── fixtures ────────────────────────────────────────────────────────────

fn lifecycle_only() -> Vec<Row> {
    vec![
        start(0),
        row(ObservationKind::Stop, "stop", "", 1),
        row(ObservationKind::Notification, "notification", "", 2),
        end(3),
    ]
}

fn small() -> Vec<Row> {
    let mut rows = vec![start(0), prompt("fix the flaky scheduler test", 1)];
    rows.extend(tool_pair("Bash", 2));
    // A call that never completed (cancelled): counted by nothing, but its
    // name still reaches the handoff.
    rows.push(row(ObservationKind::PreToolUse, "Grep", "", 3));
    rows.extend(tool_pair("tool file", 4));
    rows.push(row(ObservationKind::Stop, "stop", "", 6));
    rows.push(end(7));
    rows
}

/// `n` rows: a start, one prompt, then alternating tool pairs.
fn of_length(n: usize) -> Vec<Row> {
    let mut rows = vec![start(0), prompt("grow the list to a boundary size", 1)];
    let mut at = 2;
    while rows.len() < n {
        let [pre, post] = tool_pair(if at % 4 == 0 { "Bash" } else { "Edit" }, at);
        rows.push(pre);
        if rows.len() < n {
            rows.push(post);
        }
        at += 2;
    }
    rows.truncate(n);
    rows
}

#[derive(Clone, Copy)]
enum Ending {
    /// Stop, then SessionEnd.
    Normal,
    /// The last completed tool call touched files and nothing followed.
    FileToolNoStop,
    /// The last prompt is an unanswered question.
    QuestionNoStop,
    /// A Stop and no SessionEnd.
    StopNoEnd,
}

/// A few thousand mixed rows: prompts of every kind the heuristics tell apart,
/// both tool families and real tool names, ties, stops with and without
/// assistant text, long multi-byte titles and extension events.
fn large(n: usize, ending: Ending) -> Vec<Row> {
    let long = "長い題名".repeat(40);
    let mut rows = vec![
        start(0),
        // A second start: the newest one is the page's `started_at`.
        start(1),
        prompt("make the backpressure test deterministic", 2),
    ];
    let mut at = 3_i64;
    let mut seed = 7_u64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize
    };
    while rows.len() < n.saturating_sub(8) {
        match next() % 12 {
            0 => rows.push(prompt(&format!("question number {at}?"), at)),
            1 => rows.push(prompt("ok", at)),
            2 => rows.push(row(
                ObservationKind::UserPrompt,
                "<task-notification>",
                TASK_NOTIFICATION,
                at,
            )),
            3 => rows.push(prompt(&format!("{long} {at}"), at)),
            4 => {
                let mut r = row(ObservationKind::Other, "extension event", "", at);
                r.extension = Some("ext");
                r.source_event = Some("evt");
                rows.push(r);
            }
            5 => rows.push(row(
                ObservationKind::Stop,
                "stop",
                if next() % 2 == 0 {
                    "assistant said so"
                } else {
                    ""
                },
                at,
            )),
            6 => rows.extend(tool_pair("tool file", at)),
            7 => rows.extend(tool_pair("tool non-file", at)),
            8 => rows.extend(tool_pair("Bash", at)),
            9 => rows.extend(tool_pair("Edit", at)),
            10 if next() % 3 == 0 => rows.push(row(ObservationKind::PreToolUse, "Grep", "", at)),
            10 => {
                // A tie: three rows at one instant.
                rows.push(row(ObservationKind::Notification, "notification", "", at));
                rows.push(row(ObservationKind::PreToolUse, "Read", "", at));
                rows.push(row(ObservationKind::PostToolUse, "Read", "", at));
            }
            _ => rows.push(prompt(&format!("continue with step {at}"), at)),
        }
        at += 1;
    }
    let tail: Vec<Row> = match ending {
        Ending::Normal => vec![
            prompt("thanks", at),
            row(ObservationKind::Stop, "stop", "all done", at + 1),
            end(at + 2),
        ],
        Ending::FileToolNoStop => {
            let mut v = vec![prompt("edit the file", at)];
            v.extend(tool_pair("tool file", at + 1));
            v
        }
        Ending::QuestionNoStop => vec![prompt("is this the right approach?", at)],
        Ending::StopNoEnd => vec![
            prompt("keep going", at),
            row(ObservationKind::Stop, "stop", "", at + 1),
        ],
    };
    rows.extend(tail);
    rows
}

fn markup_only_prompts() -> Vec<Row> {
    let mut rows = vec![start(0)];
    for i in 0..3 {
        rows.push(row(
            ObservationKind::UserPrompt,
            "<task-notification>",
            TASK_NOTIFICATION,
            1 + i,
        ));
    }
    rows.extend(tool_pair("tool file", 5));
    rows.extend(tool_pair("Bash", 7));
    rows
}

fn subagent_preamble(body: &str) -> Vec<Row> {
    vec![
        start(0),
        row(ObservationKind::UserPrompt, SUBAGENT_PREAMBLE, body, 1),
        row(ObservationKind::PostToolUse, "Read", "", 2),
    ]
}

fn tools_only() -> Vec<Row> {
    let mut rows = vec![start(0)];
    rows.extend(tool_pair("tool non-file", 1));
    rows.extend(tool_pair("Edit", 3));
    rows
}

/// The last prompt has no text at all, and a question sits before it.
fn empty_last_prompt() -> Vec<Row> {
    vec![
        start(0),
        prompt("where does the writer actor start?", 1),
        row(ObservationKind::UserPrompt, "", "", 2),
        row(ObservationKind::PostToolUse, "Read", "", 3),
    ]
}

/// The newest `Stop` with a body holds only whitespace: SQL calls it
/// non-empty, the page trims it away, so the answer is the earlier one.
fn whitespace_stop() -> Vec<Row> {
    vec![
        start(0),
        prompt("summarise the change", 1),
        row(ObservationKind::Stop, "stop", "the real answer", 2),
        row(ObservationKind::PostToolUse, "Bash", "", 3),
        row(ObservationKind::Stop, "stop", "  \t\n", 4),
        end(5),
    ]
}

/// The newest rows share one instant: insertion order decides which is last.
fn tied_tail() -> Vec<Row> {
    vec![
        start(0),
        prompt("first", 1),
        prompt("what about the tail?", 9),
        prompt("a later statement", 9),
        row(ObservationKind::PostToolUse, "tool file", "", 9),
        row(ObservationKind::PostToolUse, "Bash", "", 9),
    ]
}

/// Titles the title rule must skip: kind names, scaffolding, family labels.
fn scaffolding_titles() -> Vec<Row> {
    vec![
        start(0),
        row(ObservationKind::UserPrompt, "<ide_opened_file>noise", "", 1),
        row(ObservationKind::Stop, "stop", "", 2),
        row(ObservationKind::Notification, "notification", "", 3),
        row(ObservationKind::PostToolUse, "tool file", "", 4),
        row(ObservationKind::Other, "A real title", "", 5),
    ]
}

// ── the tests ───────────────────────────────────────────────────────────

/// Page, handoff (both flavours) and the lifecycle-only decision are the same
/// whether they come from the bounded reads or from the full row load, on every
/// small and boundary shape.
#[tokio::test]
async fn small_and_boundary_sessions_render_identically() {
    let fx = fixture().await;
    let empty = fx.seed(&[]).await;
    fx.assert_equivalent("empty", empty).await;
    for (name, rows) in [
        ("lifecycle-only", lifecycle_only()),
        ("small", small()),
        ("499 rows", of_length(RAW_CAP - 1)),
        ("500 rows (whole list)", of_length(RAW_CAP)),
        ("501 rows (one omitted)", of_length(RAW_CAP + 1)),
        ("600 rows", of_length(600)),
        ("markup-only prompts", markup_only_prompts()),
        (
            "preamble with a usable body",
            subagent_preamble("\n\n  <ide_opened_file>x\n  Review the diff in src/lib.rs\nmore"),
        ),
        (
            "preamble with no usable body",
            subagent_preamble("<ide_opened_file>x\n\n"),
        ),
        ("tools only", tools_only()),
        ("empty last prompt", empty_last_prompt()),
        ("whitespace-only last stop", whitespace_stop()),
        ("tied tail", tied_tail()),
        ("scaffolding titles", scaffolding_titles()),
    ] {
        let sid = fx.seed(&rows).await;
        fx.assert_equivalent(name, sid).await;
    }
}

/// A couple of thousand mixed rows, with each way a session can end.
#[tokio::test]
async fn a_large_mixed_session_renders_identically() {
    let fx = fixture().await;
    for (name, ending) in [
        ("normal end", Ending::Normal),
        ("file tool, no stop", Ending::FileToolNoStop),
        ("unanswered question", Ending::QuestionNoStop),
        ("stop without end", Ending::StopNoEnd),
    ] {
        let sid = fx.seed(&large(2_500, ending)).await;
        fx.assert_equivalent(name, sid).await;
    }
}

/// Nothing about a session's digest grows with the session: the raw lines
/// and the prompt-title slice are capped, and the rest is counts and
/// early-exit scans.
#[tokio::test]
async fn a_huge_session_reads_a_bounded_number_of_rows() {
    let fx = fixture().await;
    let rows = large(12_000, Ending::Normal);
    let prompts = rows
        .iter()
        .filter(|r| r.kind == ObservationKind::UserPrompt)
        .count();
    let sid = fx.seed(&rows).await;
    assert!(
        rows.len() > 20 * RAW_CAP,
        "the fixture must dwarf the bound"
    );

    let (digest, stats) = SessionDigest::load_with_stats(&fx.store.reader, fx.ws, fx.proj, sid)
        .await
        .unwrap();

    assert_eq!(digest.observation_count(), rows.len());
    assert!(
        stats.rows_read <= RAW_CAP + SCAN_SLACK,
        "a {}-row session read {} rows; the page prints at most {RAW_CAP} lines",
        rows.len(),
        stats.rows_read
    );
    assert_eq!(stats.prompt_titles, prompts);

    // Control: the load this replaced materializes every row.
    let full = fx.store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(full.len(), rows.len());
    // And the output did not change for being cheap.
    fx.assert_equivalent("12k rows", sid).await;
}

/// SQL and Rust apply the same header rule: the bounded reads and the row-list
/// digest agree on the count and the newest failure, an echoed body is not a
/// failure, and the newest failure is a seek, not a walk.
#[tokio::test]
async fn failed_tool_facts_match_the_row_list_and_stay_bounded() {
    let fx = fixture().await;
    let ok = "tool_family: non-file\ntool_call_id: toolu_ok\noutcome: unknown\n---\n42 passing";
    let mut rows = vec![
        row(
            ObservationKind::UserPrompt,
            "fix the build",
            "fix the build",
            0,
        ),
        row(
            ObservationKind::PostToolUse,
            "tool non-file: Bash",
            "tool_family: non-file\ntool_call_id: toolu_1\noutcome: error\n---\n\
             Exit code 1\nError: Cannot find module 'express'",
            1,
        ),
    ];
    for i in 0..RAW_CAP as i64 {
        rows.push(row(
            ObservationKind::PostToolUse,
            "tool non-file: Bash",
            ok,
            2 + i,
        ));
    }
    let echoed_at = 2 + RAW_CAP as i64;
    rows.push(row(
        ObservationKind::PostToolUse,
        "tool non-file: Bash",
        "tool_family: non-file\ntool_call_id: toolu_echo\noutcome: unknown\n---\n\
         tool_family: file\noutcome: error\n---\nquoted from a page\noutcome: error",
        echoed_at,
    ));
    rows.push(row(
        ObservationKind::PostToolUse,
        "tool unknown",
        "tool_family: unknown\nmcp_server: aim\nmcp_tool: memory_query\noutcome: error",
        echoed_at + 1,
    ));
    for i in 0..RAW_CAP as i64 {
        rows.push(row(
            ObservationKind::PostToolUse,
            "tool non-file: Bash",
            ok,
            echoed_at + 2 + i,
        ));
    }
    let sid = fx.seed(&rows).await;

    let (digest, stats) = SessionDigest::load_with_stats(&fx.store.reader, fx.ws, fx.proj, sid)
        .await
        .unwrap();
    assert_eq!(digest.failed_tool_calls(), 2);
    let newest = digest
        .handoff_facts()
        .last_failed_tool
        .clone()
        .expect("the newest failure");
    assert_eq!(newest.title, "tool unknown");
    assert_eq!(newest.error, "");
    assert!(
        stats.rows_read <= RAW_CAP + SCAN_SLACK,
        "a {}-row session read {} rows; the newest failure must be a seek, not a walk",
        rows.len(),
        stats.rows_read
    );

    let full = fx.store.reader.observations_for_session(sid).await.unwrap();
    let from_rows = SessionDigest::from_observations(sid, &full);
    assert_eq!(from_rows.failed_tool_calls(), digest.failed_tool_calls());
    assert_eq!(from_rows.handoff_facts().last_failed_tool, Some(newest));

    // Control: a session ending on a real failure quotes its error.
    let sid = fx.seed(&rows[..2]).await;
    let digest = SessionDigest::load(&fx.store.reader, fx.ws, fx.proj, sid)
        .await
        .unwrap();
    assert_eq!(digest.failed_tool_calls(), 1);
    assert_eq!(
        digest
            .handoff_facts()
            .last_failed_tool
            .as_ref()
            .map(|failed| failed.describe()),
        Some("Bash: Exit code 1 Error: Cannot find module 'express'".to_string())
    );
}

/// Subagent preambles carry the only prompt bodies a SessionEnd reads, and a
/// session can hold thousands of them: the title is settled by the first
/// usable one, so the bodies after it are never read.
#[tokio::test]
async fn subagent_preamble_bodies_stop_at_the_first_usable_one() {
    let fx = fixture().await;
    let filler = "x".repeat(4096);
    let unusable = format!("<ide_opened_file>{filler}\n\n");
    let usable = format!("Review the diff in src/lib.rs\n{filler}");
    let preambles = 2_000;
    for (name, leading_unusable) in [("first usable", 0), ("three unusable first", 3)] {
        let mut rows = vec![start(0)];
        for i in 0..preambles {
            let body = if i < leading_unusable {
                &unusable
            } else {
                &usable
            };
            rows.push(row(
                ObservationKind::UserPrompt,
                SUBAGENT_PREAMBLE,
                body,
                1 + i as i64,
            ));
        }
        rows.push(end(1 + preambles as i64));
        let sid = fx.seed(&rows).await;

        let (_, stats) = SessionDigest::load_with_stats(&fx.store.reader, fx.ws, fx.proj, sid)
            .await
            .unwrap();
        assert_eq!(
            stats.prompt_titles, preambles,
            "{name}: every title is kept"
        );
        assert_eq!(
            stats.prompt_bodies_read,
            leading_unusable + 1,
            "{name}: bodies read past the one that named the page"
        );
        fx.assert_equivalent(name, sid).await;
    }
}

/// The `## Prompts` list is capped the way the raw-observation list is: a
/// session with more prompts than the cap shows its first and last slice,
/// numbered with the real positions, with one omission line between them,
/// while the count and the stats keep the true total.
#[tokio::test]
async fn the_prompt_list_is_capped_like_the_raw_observations() {
    use crate::synth::{PROMPT_TITLES_HEAD, PROMPT_TITLES_TAIL};

    let fx = fixture().await;
    let prompts = 2_000;
    let mut rows = vec![start(0)];
    for i in 1..=prompts {
        rows.push(row(
            ObservationKind::UserPrompt,
            &format!("prompt {i}"),
            "",
            i as i64,
        ));
    }
    rows.push(end(prompts as i64 + 1));
    let sid = fx.seed(&rows).await;

    // The store read itself: at most the bounded slice plus the true count.
    let capped = fx
        .store
        .reader
        .read_session_observations(fx.ws, fx.proj, sid, |s| {
            s.prompt_titles(PROMPT_TITLES_HEAD, PROMPT_TITLES_TAIL)
        })
        .await
        .unwrap();
    assert_eq!(capped.total, prompts);
    assert_eq!(capped.first.len(), PROMPT_TITLES_HEAD);
    assert_eq!(capped.last.len(), PROMPT_TITLES_TAIL);
    assert_eq!(capped.first[0], "prompt 1");
    assert_eq!(
        capped.first[PROMPT_TITLES_HEAD - 1],
        format!("prompt {PROMPT_TITLES_HEAD}")
    );
    assert_eq!(
        capped.last[0],
        format!("prompt {}", prompts - PROMPT_TITLES_TAIL + 1)
    );
    assert_eq!(
        capped.last[PROMPT_TITLES_TAIL - 1],
        format!("prompt {prompts}")
    );

    // Through the real SessionEnd path: the stats keep the true total...
    let (digest, stats) = SessionDigest::load_with_stats(&fx.store.reader, fx.ws, fx.proj, sid)
        .await
        .unwrap();
    assert_eq!(stats.prompt_titles, prompts);

    // ...and the page prints exactly the bounded slice, with the numbers and
    // the omission line.
    let page = crate::synthesize_session_page(fx.ws, fx.proj, sid, AgentKind::ClaudeCode, &digest);
    let section = page
        .body
        .split("## Prompts\n\n")
        .nth(1)
        .expect("the page lists its prompts")
        .split("\n## Raw observations")
        .next()
        .unwrap();
    let numbered = section
        .lines()
        .filter(|l| l.split('.').next().unwrap_or("").parse::<usize>().is_ok())
        .count();
    assert_eq!(numbered, PROMPT_TITLES_HEAD + PROMPT_TITLES_TAIL);
    assert!(section.contains("1. prompt 1\n"));
    assert!(section.contains("250. prompt 250\n"));
    assert!(section.contains("1751. prompt 1751\n"));
    assert!(section.contains("2000. prompt 2000\n"));
    assert!(
        section.contains(
            "_... 1500 prompts omitted from the middle (showing first 250 and last 250)._"
        ),
        "one omission line with the true gap: {section}"
    );
    // The count line keeps the true total.
    let summary = page.frontmatter_json["summary"].as_str().unwrap();
    assert!(summary.starts_with("2000 prompts"), "count line: {summary}");
    // And the title, the handoff and the rest of the page are the oracle's.
    assert_eq!(page.title, "prompt 1");
    fx.assert_equivalent("2000 prompts (capped list)", sid)
        .await;
}

/// Exactly at the cap the page still lists every prompt; one over it shows
/// the head and tail slices and the omission line.
#[tokio::test]
async fn the_prompt_cap_boundary_keeps_five_hundred_and_cuts_five_hundred_one() {
    for (n, capped) in [(500, false), (501, true)] {
        let fx = fixture().await;
        let mut rows = vec![start(0)];
        for i in 1..=n {
            rows.push(row(
                ObservationKind::UserPrompt,
                &format!("p{i}"),
                "",
                i as i64,
            ));
        }
        rows.push(end(n as i64 + 1));
        let sid = fx.seed(&rows).await;
        let (digest, stats) = SessionDigest::load_with_stats(&fx.store.reader, fx.ws, fx.proj, sid)
            .await
            .unwrap();
        assert_eq!(
            stats.prompt_titles, n,
            "n={n}: the stats keep the true total"
        );
        let page =
            crate::synthesize_session_page(fx.ws, fx.proj, sid, AgentKind::ClaudeCode, &digest);
        if capped {
            assert!(page.body.contains("250. p250\n"), "n={n}");
            assert!(
                !page.body.contains("251. p251\n"),
                "n={n}: the gap drops p251"
            );
            assert!(page.body.contains("252. p252\n"), "n={n}");
            assert!(page.body.contains("501. p501\n"), "n={n}");
            assert!(
                page.body.contains(
                    "_... 1 prompts omitted from the middle (showing first 250 and last 250)._"
                ),
                "n={n}: one omission line"
            );
        } else {
            assert!(page.body.contains("500. p500\n"), "n={n}");
            assert!(
                !page.body.contains("prompts omitted"),
                "n={n}: whole list, no marker"
            );
        }
        let summary = page.frontmatter_json["summary"].as_str().unwrap();
        assert!(
            summary.starts_with(&format!("{n} prompts")),
            "n={n}: count line: {summary}"
        );
        fx.assert_equivalent(&format!("prompts={n}"), sid).await;
    }
}

/// The Rust predicate the original code used and the existence query the
/// router asks now classify every observation kind alike. (The writer's
/// lifecycle-only re-check is pinned to the same query in
/// `ops::tests::the_lifecycle_only_verdict_follows_the_reader_predicate`.)
#[tokio::test]
async fn the_ephemeral_predicate_agrees_for_every_observation_kind() {
    // An exhaustive match: a new kind cannot be added without deciding here.
    fn all_kinds(kind: ObservationKind) -> bool {
        match kind {
            ObservationKind::SessionStart
            | ObservationKind::UserPrompt
            | ObservationKind::PreToolUse
            | ObservationKind::PostToolUse
            | ObservationKind::PreCompact
            | ObservationKind::PostCompaction
            | ObservationKind::Notification
            | ObservationKind::Stop
            | ObservationKind::SessionEnd
            | ObservationKind::Other => true,
        }
    }
    let kinds = [
        ObservationKind::SessionStart,
        ObservationKind::UserPrompt,
        ObservationKind::PreToolUse,
        ObservationKind::PostToolUse,
        ObservationKind::PreCompact,
        ObservationKind::PostCompaction,
        ObservationKind::Notification,
        ObservationKind::Stop,
        ObservationKind::SessionEnd,
        ObservationKind::Other,
    ];
    assert!(kinds.iter().all(|k| all_kinds(*k)));

    let fx = fixture().await;
    for kind in kinds {
        let rows = [row(kind, "title", "body", 0)];
        let sid = fx.seed(&rows).await;
        let materialized = fx.store.reader.observations_for_session(sid).await.unwrap();
        let rust = !is_ephemeral_session(&materialized);
        let sql = fx
            .store
            .reader
            .session_has_substantive_observation(fx.ws, fx.proj, sid)
            .await
            .unwrap();
        let digest = SessionDigest::load(&fx.store.reader, fx.ws, fx.proj, sid)
            .await
            .unwrap();
        assert_eq!(sql, rust, "{kind:?}: existence query vs row predicate");
        assert_eq!(
            digest.is_substantive(),
            rust,
            "{kind:?}: digest vs row predicate"
        );
    }
}
