//! View data shared by project, harness and session diagnostics.

use crate::templates::{
    SessionRow, TokenUsagePanel, TokenUsageRow, agent_label, duration_between, fmt_tokens,
    humanize, project_href, session_href, short_id,
};
use ai_memory_core::AgentKind;
use ai_memory_store::{AgentUsage, SessionSummary, TokenUsageReport};

pub(crate) struct AgentCard {
    pub kind: String,
    pub label: &'static str,
    pub href: String,
    pub compose_href: String,
    pub sessions: u64,
    pub open: u64,
    pub measured: u64,
    pub coverage: u64,
    pub calls: u64,
    pub tokens: String,
    pub input: String,
    pub output: String,
    pub cache: String,
}

pub(crate) fn agent_card(row: AgentUsage, workspace: &str, project: &str) -> AgentCard {
    let href = format!("{}/agents/{}", project_href(workspace, project), row.agent);
    AgentCard {
        label: agent_label(AgentKind::from_wire(&row.agent)),
        kind: row.agent.clone(),
        href,
        compose_href: format!(
            "inbox?workspace={}&project={}&agent={}#compose",
            crate::templates::encode_segment(workspace),
            crate::templates::encode_segment(project),
            row.agent
        ),
        sessions: row.sessions,
        open: row.open,
        measured: row.measured_sessions,
        coverage: row
            .measured_sessions
            .saturating_mul(100)
            .checked_div(row.sessions)
            .unwrap_or(0),
        calls: row.calls,
        tokens: if row.measured_sessions == 0 {
            "—".into()
        } else {
            fmt_tokens(row.input + row.output + row.cache_read + row.cache_write)
        },
        input: fmt_tokens(row.input),
        output: fmt_tokens(row.output),
        cache: fmt_tokens(row.cache_read + row.cache_write),
    }
}

pub(crate) fn session_row(s: SessionSummary, workspace: &str, project: &str) -> SessionRow {
    let id = s.session_id.to_string();
    SessionRow {
        agent_label: agent_label(AgentKind::from_wire(&s.agent_kind)),
        duration: duration_between(&s.started_at, s.ended_at.as_deref()),
        tokens: s
            .usage
            .as_ref()
            .map(|u| {
                format!(
                    "{} in / {} out",
                    fmt_tokens(u.input_tokens),
                    fmt_tokens(u.output_tokens)
                )
            })
            .unwrap_or_else(|| "—".into()),
        href: session_href(workspace, project, &id),
        agent_kind: s.agent_kind,
        short_id: short_id(&id),
        observation_count: s.observation_count,
        started_relative: humanize(&s.started_at),
        open: s.ended_at.is_none(),
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct UsageQuery {
    #[serde(default)]
    pub calls_q: String,
    #[serde(default)]
    pub calls_sort: String,
    #[serde(default)]
    pub calls_signal: String,
    #[serde(default, deserialize_with = "query_offset")]
    pub calls_offset: usize,
    #[serde(default)]
    pub calls_model: String,
    #[serde(default)]
    pub calls_agent: String,
    #[serde(default)]
    pub calls_session: String,
    #[serde(default)]
    pub calls_from: String,
    #[serde(default)]
    pub calls_until: String,
    #[serde(default)]
    pub calls_turn: String,
    #[serde(default)]
    pub calls_tool: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub sort: String,
}

// Flattened query values arrive as strings even for numeric fields.
fn query_offset<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}

fn usage_time(value: &str) -> Result<Option<i64>, axum::http::StatusCode> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 64 {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }
    let normalized = match value.len() {
        10 => format!("{value}T00:00:00Z"),
        16 => format!("{value}:00Z"),
        19 => format!("{value}Z"),
        _ => value.into(),
    };
    normalized
        .parse::<jiff::Timestamp>()
        .map(|t| Some(t.as_microsecond()))
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)
}

fn usage_time_input(value: &str) -> String {
    usage_time(value)
        .ok()
        .flatten()
        .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
        .map(|t| t.to_string().trim_end_matches('Z').to_owned())
        .unwrap_or_default()
}

impl UsageQuery {
    pub fn store_query(&self) -> Result<ai_memory_store::TokenCallQuery, axum::http::StatusCode> {
        use ai_memory_store::{TokenCallOrder, TokenCallQuery};
        let order = match self.calls_sort.as_str() {
            "" | "tokens" => TokenCallOrder::Largest,
            "recent" => TokenCallOrder::Recent,
            _ => return Err(axum::http::StatusCode::BAD_REQUEST),
        };
        let signals_only = match self.calls_signal.as_str() {
            "" | "all" => false,
            "signals" => true,
            _ => return Err(axum::http::StatusCode::BAD_REQUEST),
        };
        if self.calls_q.chars().count() > 256 {
            return Err(axum::http::StatusCode::BAD_REQUEST);
        }
        let model = (!self.calls_model.is_empty()).then(|| self.calls_model.clone());
        let agent = if self.calls_agent.is_empty() {
            None
        } else {
            Some(
                AgentKind::ALL
                    .into_iter()
                    .find(|a| a.as_str() == self.calls_agent)
                    .ok_or(axum::http::StatusCode::BAD_REQUEST)?,
            )
        };
        let session_id = if self.calls_session.is_empty() {
            None
        } else {
            Some(
                self.calls_session
                    .parse()
                    .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?,
            )
        };
        let from_us = usage_time(&self.calls_from)?;
        let until_us = usage_time(&self.calls_until)?;
        if from_us
            .zip(until_us)
            .is_some_and(|(from, until)| from >= until)
            || self.calls_model.len() > 128
            || self.calls_turn.len() > 128
            || self.calls_tool.len() > 128
        {
            return Err(axum::http::StatusCode::BAD_REQUEST);
        }
        Ok(TokenCallQuery {
            search: self.calls_q.trim().into(),
            signals_only,
            order,
            offset: self.calls_offset,
            model,
            agent,
            session_id,
            from_us,
            until_us,
            turn_id: (!self.calls_turn.is_empty()).then(|| self.calls_turn.clone()),
            tool: (!self.calls_tool.is_empty()).then(|| self.calls_tool.clone()),
        })
    }

    pub fn session_view(&self) -> Result<(bool, bool), axum::http::StatusCode> {
        let open_only = match self.state.as_str() {
            "" | "all" => false,
            "open" => true,
            _ => return Err(axum::http::StatusCode::BAD_REQUEST),
        };
        let sort_events = match self.sort.as_str() {
            "" | "recent" => false,
            "events" => true,
            _ => return Err(axum::http::StatusCode::BAD_REQUEST),
        };
        Ok((open_only, sort_events))
    }

    fn page_href(&self, base: &str, offset: usize) -> String {
        format!(
            "{base}&calls_q={}&calls_sort={}&calls_signal={}&calls_model={}&calls_agent={}&calls_session={}&calls_from={}&calls_until={}&calls_turn={}&calls_tool={}&calls_offset={offset}#token-usage",
            crate::templates::encode_segment(self.calls_q.trim()),
            crate::templates::encode_segment(&self.calls_sort),
            crate::templates::encode_segment(&self.calls_signal),
            crate::templates::encode_segment(&self.calls_model),
            crate::templates::encode_segment(&self.calls_agent),
            crate::templates::encode_segment(&self.calls_session),
            crate::templates::encode_segment(&self.calls_from),
            crate::templates::encode_segment(&self.calls_until),
            crate::templates::encode_segment(&self.calls_turn),
            crate::templates::encode_segment(&self.calls_tool)
        )
    }
}

pub(crate) fn usage_panel(
    report: TokenUsageReport,
    workspace: &str,
    project: &str,
    query: &UsageQuery,
    base: &str,
) -> TokenUsagePanel {
    let input_total = report.input + report.cache_read + report.cache_write;
    let mean = (input_total + report.output)
        .checked_div(report.call_count)
        .unwrap_or(0);
    let shown = report.calls.len() as u64;
    TokenUsagePanel {
        model: query.calls_model.clone(),
        session: query.calls_session.clone(),
        from: usage_time_input(&query.calls_from),
        until: usage_time_input(&query.calls_until),
        turn: query.calls_turn.clone(),
        tool: query.calls_tool.clone(),
        agents: AgentKind::ALL
            .into_iter()
            .map(|a| crate::templates::UsageAgentOption {
                value: a.as_str().into(),
                label: agent_label(a).into(),
                selected: query.calls_agent == a.as_str(),
            })
            .collect(),
        matching_tokens: fmt_tokens(report.matching_tokens),
        search: query.calls_q.clone(),
        recent: query.calls_sort == "recent",
        signals_only: query.calls_signal == "signals",
        matching: report.matching_calls,
        shown_start: if shown == 0 {
            0
        } else {
            query.calls_offset as u64 + 1
        },
        shown_end: if shown == 0 {
            0
        } else {
            (query.calls_offset as u64).saturating_add(shown)
        },
        previous_href: if query.calls_offset > 0 {
            query.page_href(base, query.calls_offset.saturating_sub(50))
        } else {
            String::new()
        },
        next_href: if (query.calls_offset as u64).saturating_add(shown) < report.matching_calls {
            query.page_href(base, query.calls_offset.saturating_add(50))
        } else {
            String::new()
        },
        observation_offset: 0,
        observation_limit: 50,
        count: report.call_count,
        input: fmt_tokens(report.input),
        output: fmt_tokens(report.output),
        cache_read: fmt_tokens(report.cache_read),
        cache_write: fmt_tokens(report.cache_write),
        hit_rate: if input_total > 0 {
            format!(
                "{:.0}%",
                report.cache_read as f64 / input_total as f64 * 100.0
            )
        } else {
            "—".into()
        },
        rows: report
            .calls
            .into_iter()
            .map(|c| {
                let context = c.input + c.cache_read + c.cache_write;
                let total = context + c.output;
                let signal = if report.call_count >= 5 && total > mean.saturating_mul(3) {
                    "Above 3× average"
                } else if context >= 100_000 {
                    "Large context"
                } else if c.output >= 20_000 {
                    "Long output"
                } else {
                    "—"
                };
                let mut drilldown = query.clone();
                drilldown.calls_session = c.session_id.clone();
                drilldown.calls_turn = c.turn_id.clone().unwrap_or_default();
                let turn_href = drilldown.page_href(base, 0);
                let tools = c
                    .tools
                    .into_iter()
                    .map(|t| {
                        let mut tool_query = drilldown.clone();
                        tool_query.calls_tool = t.name.clone();
                        crate::templates::UsageTool {
                            id: t.id,
                            name: t.name,
                            relation: match t.relation {
                                ai_memory_core::TokenToolRelation::Requested => {
                                    "Requested by this response"
                                }
                                ai_memory_core::TokenToolRelation::Input => {
                                    "Result supplied to this response"
                                }
                                ai_memory_core::TokenToolRelation::Turn => {
                                    "Recorded in the same turn (association)"
                                }
                            },
                            is_error: t.is_error,
                            href: tool_query.page_href(base, 0),
                        }
                    })
                    .collect();
                let recorded_at = c
                    .response_at
                    .clone()
                    .unwrap_or_else(|| c.observed_at.clone());
                TokenUsageRow {
                    turn: c.turn_id.unwrap_or_default(),
                    turn_href,
                    response_native: c.response_id.unwrap_or_default(),
                    session: c.session_id.clone(),
                    time_basis: if c.response_at.is_some() {
                        "Reported response time"
                    } else {
                        "Capture time; response time unknown"
                    },
                    recorded_at: recorded_at.clone(),
                    tools,
                    total_value: total,
                    observed_at: c.observed_at.clone(),
                    href: session_href(workspace, project, &c.session_id),
                    id: c.call_id,
                    model: c.model,
                    agent: agent_label(AgentKind::from_wire(&c.agent)).into(),
                    input: fmt_tokens(c.input),
                    output: fmt_tokens(c.output),
                    cache: format!(
                        "{} read / {} write",
                        fmt_tokens(c.cache_read),
                        fmt_tokens(c.cache_write)
                    ),
                    total: fmt_tokens(total),
                    when: humanize(&recorded_at),
                    signal: signal.into(),
                }
            })
            .collect(),
    }
}
