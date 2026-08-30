//! Rule-based session-page synthesis (no LLM).
//!
//! At `SessionEnd` we already have N observation rows for the session.
//! We turn them into a single markdown page under `wiki/sessions/<id>.md`
//! using only deterministic heuristics: first-prompt as title, files
//! touched, tool-call counts. Once the LLM provider lands in M6 we'll
//! add an opt-in path that re-narrates the page.

use std::collections::BTreeMap;

use ai_memory_core::{
    AgentKind, NewPage, Observation, ObservationKind, PagePath, ProjectId, SessionId, Tier,
    WorkspaceId, looks_like_scaffolding,
};
use jiff::tz::TimeZone;

use crate::payload::{is_safe_tool_title, truncate_for_title};

const RAW_OBSERVATION_MAX_LINES: usize = 500;
const RAW_OBSERVATION_HEAD_LINES: usize = 250;
const RAW_OBSERVATION_TAIL_LINES: usize = RAW_OBSERVATION_MAX_LINES - RAW_OBSERVATION_HEAD_LINES;
const SUBAGENT_PROMPT_PREAMBLE: &str = "You are a subagent spawned by another session.";

/// Build a [`NewPage`] from the observations collected during a session.
///
/// The returned page is *always* under `sessions/<session-id>.md` so each
/// session has a stable URL the user can bookmark.
#[must_use]
pub fn synthesize_session_page(
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
        if obs.kind == ObservationKind::SessionStart {
            continue;
        }
        if !obs.title.is_empty() && !looks_like_title_scaffolding(&obs.title) {
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
    #[test]
    fn a_model_id_on_session_start_never_becomes_the_title() {
        let sid = test_session_id();
        let with_a_later_title = vec![
            obs(ObservationKind::SessionStart, "claude-opus-5[1m]"),
            obs(ObservationKind::UserPrompt, ""),
            obs(ObservationKind::PostToolUse, "tool non-file"),
        ];
        assert_eq!(derive_title(&with_a_later_title, sid), "tool non-file");

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
        let body = render_body(test_session_id(), &obs, &title, &tally);
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
