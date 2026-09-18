//! Query the eval server through the real MCP surface — the same
//! Streamable-HTTP `tools/call memory_query` an agent uses. Explicit
//! `workspace` + `project` args scope each call to one question's
//! haystack (the documented pattern for static MCP clients).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::server::EVAL_AUTH_TOKEN;

/// One retrieval hit after flattening pages + raw observations, in
/// rank order (index 0 = best).
#[derive(Debug, Clone)]
pub struct Retrieved {
    /// `sessions/<uuid>.md` page or raw observation → owning session
    /// uuid; other pages have no session provenance.
    pub session_uuid: Option<uuid::Uuid>,
}

/// The title + snippet text of one retrieval hit, in rank order. This is
/// exactly what `memory_query` puts in front of the agent, and the context
/// the R2b QA answer-synthesizer reads to produce a candidate answer.
#[derive(Debug, Clone)]
pub struct HitContext {
    /// Hit title (page title or raw observation title).
    pub title: String,
    /// Hit snippet (the bounded excerpt the agent would read).
    pub snippet: String,
}

/// Outcome of one `memory_query` call: the ranked hits plus the two
/// R2 observability signals — how long the round trip took and how much
/// context the agent would have to read to consume the result.
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    /// Flattened ranked session attributions (index 0 = best).
    pub retrieved: Vec<Retrieved>,
    /// Ranked hit contexts (title + snippet), same order as `retrieved`.
    /// The QA answer-synthesizer (R2b) reads these; the R2a scoring path
    /// ignores them, so the default zero-LLM run is unaffected.
    pub contexts: Vec<HitContext>,
    /// A server-provided answer, when a (future) `answer=true` feature
    /// populates a top-level `answer` field on the `memory_query` result.
    /// `None` today; the QA path falls back to harness synthesis when it is
    /// absent or blank. Detecting it now future-proofs R2b for the B4
    /// `answer`/`reasoning` MCP features without a harness change.
    pub answer: Option<String>,
    /// Wall-clock of the `memory_query` MCP round trip (request send →
    /// response parsed).
    pub latency: Duration,
    /// Estimated tokens an agent would ingest from this result: the total
    /// character count of every returned hit's `title` + `snippet`
    /// (pages and raw observations), divided by 4 (the documented
    /// chars/4 heuristic). The snippet is exactly what `memory_query`
    /// puts in front of the agent, so this is the context cost of the
    /// retrieval, not of the underlying full pages.
    pub context_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct QueryResponse {
    #[serde(default)]
    hits: Vec<PageHitLite>,
    #[serde(default)]
    raw_hits: Vec<RawHitLite>,
    /// Optional server-synthesized answer (future `answer=true` feature).
    /// Unknown fields are ignored by serde, so this is forward-compatible:
    /// today's server simply never sets it.
    #[serde(default)]
    answer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageHitLite {
    path: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    snippet: String,
}

#[derive(Debug, Deserialize)]
struct RawHitLite {
    session_id: uuid::Uuid,
    #[serde(default)]
    title: String,
    #[serde(default)]
    snippet: String,
}

/// Divisor for the chars/4 token estimator (see [`QueryOutcome::context_tokens`]).
const CHARS_PER_TOKEN: usize = 4;

/// chars/4 token estimate for one string (Unicode scalar count).
fn estimate_tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Call `memory_query` and flatten the response into ranked session
/// attributions: compiled-page hits first (they are the primary answer
/// surface), then raw observation hits, preserving each list's order.
pub async fn memory_query(
    client: &reqwest::Client,
    base_url: &str,
    workspace: &str,
    project: &str,
    query: &str,
    limit: usize,
    extra_args: &serde_json::Map<String, serde_json::Value>,
) -> Result<QueryOutcome> {
    // Extra config-supplied args first, then the scoping/limit the harness
    // controls, so a config can A/B a `memory_query` knob (e.g. a future
    // `pin_first`/`include_superseded`) without ever redirecting the query
    // off its question's private haystack.
    let mut arguments = extra_args.clone();
    arguments.insert("query".into(), json!(query));
    arguments.insert("workspace".into(), json!(workspace));
    arguments.insert("project".into(), json!(project));
    arguments.insert("limit".into(), json!(limit));
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_query",
            "arguments": arguments,
        }
    });
    let started = Instant::now();
    let resp = client
        .post(format!("{base_url}/mcp"))
        .bearer_auth(EVAL_AUTH_TOKEN)
        .header("accept", "application/json, text/event-stream")
        .json(&request)
        .send()
        .await
        .context("posting MCP tools/call")?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("MCP call failed ({status}): {body}");
    }

    let rpc: serde_json::Value = if content_type.starts_with("text/event-stream") {
        // Streamable HTTP may frame the response as SSE; the JSON-RPC
        // response is the last `data:` payload carrying our id.
        let mut last = None;
        for line in body.lines() {
            if let Some(data) = line.strip_prefix("data:")
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(data.trim())
                && v.get("id").is_some()
            {
                last = Some(v);
            }
        }
        last.ok_or_else(|| anyhow::anyhow!("no JSON-RPC response in SSE stream: {body}"))?
    } else {
        serde_json::from_str(&body).with_context(|| format!("parsing MCP response: {body}"))?
    };

    if let Some(err) = rpc.get("error") {
        bail!("MCP error: {err}");
    }
    let result = rpc
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("MCP response missing result: {rpc}"))?;
    if result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        bail!("memory_query tool error: {result}");
    }
    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("memory_query returned no text content: {result}"))?;
    let parsed: QueryResponse =
        serde_json::from_str(text).with_context(|| format!("parsing memory_query JSON: {text}"))?;
    // A blank/whitespace `answer` is treated as absent so the QA path falls
    // back to harness synthesis rather than grading an empty string.
    let answer = parsed
        .answer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let (retrieved, contexts, context_tokens) = flatten(parsed);
    Ok(QueryOutcome {
        retrieved,
        contexts,
        answer,
        latency: started.elapsed(),
        context_tokens,
    })
}

/// Flatten pages + raw hits into ranked session attributions and their
/// title+snippet contexts, and sum the context-token estimate over every
/// hit's `title` + `snippet`.
fn flatten(resp: QueryResponse) -> (Vec<Retrieved>, Vec<HitContext>, usize) {
    let mut out = Vec::new();
    let mut contexts = Vec::new();
    let mut chars = 0usize;
    for hit in resp.hits {
        chars += hit.title.chars().count() + hit.snippet.chars().count();
        out.push(Retrieved {
            session_uuid: session_uuid_from_path(&hit.path),
        });
        contexts.push(HitContext {
            title: hit.title,
            snippet: hit.snippet,
        });
    }
    for hit in resp.raw_hits {
        chars += hit.title.chars().count() + hit.snippet.chars().count();
        out.push(Retrieved {
            session_uuid: Some(hit.session_id),
        });
        contexts.push(HitContext {
            title: hit.title,
            snippet: hit.snippet,
        });
    }
    (out, contexts, estimate_tokens(chars))
}

/// `sessions/<uuid>.md` → the owning session; anything else → None.
fn session_uuid_from_path(path: &str) -> Option<uuid::Uuid> {
    let stem = path.strip_prefix("sessions/")?.strip_suffix(".md")?;
    uuid::Uuid::parse_str(stem).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_pages_attribute_and_other_pages_do_not() {
        let sid = uuid::Uuid::now_v7();
        assert_eq!(
            session_uuid_from_path(&format!("sessions/{sid}.md")),
            Some(sid)
        );
        assert_eq!(session_uuid_from_path("gotchas/build.md"), None);
        assert_eq!(session_uuid_from_path("sessions/not-a-uuid.md"), None);
    }

    #[test]
    fn pages_rank_before_raw_hits_and_order_is_preserved() {
        let a = uuid::Uuid::now_v7();
        let b = uuid::Uuid::now_v7();
        let resp = QueryResponse {
            hits: vec![PageHitLite {
                path: format!("sessions/{a}.md"),
                title: String::new(),
                snippet: String::new(),
            }],
            raw_hits: vec![RawHitLite {
                session_id: b,
                title: String::new(),
                snippet: String::new(),
            }],
            answer: None,
        };
        let (flat, contexts, _) = flatten(resp);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].session_uuid, Some(a));
        assert_eq!(flat[1].session_uuid, Some(b));
        // Contexts track the flattened hits one-for-one, in the same order.
        assert_eq!(contexts.len(), 2);
    }

    #[test]
    fn context_tokens_sum_title_and_snippet_over_all_hits() {
        let a = uuid::Uuid::now_v7();
        let b = uuid::Uuid::now_v7();
        let resp = QueryResponse {
            // 4 + 4 = 8 chars → 2 tokens
            hits: vec![PageHitLite {
                path: format!("sessions/{a}.md"),
                title: "abcd".into(),
                snippet: "efgh".into(),
            }],
            // 4 + 4 = 8 chars → +2 tokens
            raw_hits: vec![RawHitLite {
                session_id: b,
                title: "ijkl".into(),
                snippet: "mnop".into(),
            }],
            answer: None,
        };
        let (_, contexts, tokens) = flatten(resp);
        assert_eq!(tokens, 4);
        // Snippets are preserved verbatim for the QA synthesizer.
        assert_eq!(contexts[0].title, "abcd");
        assert_eq!(contexts[0].snippet, "efgh");
        assert_eq!(contexts[1].snippet, "mnop");
    }

    #[test]
    fn token_estimate_rounds_up() {
        // 5 chars / 4 = 1.25 → ceil 2
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(8), 2);
    }
}
