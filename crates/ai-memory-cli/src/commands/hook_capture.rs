//! Native lifecycle-hook capture helpers.
//!
//! Mirrors the POSIX `hooks/lib/_lib.sh` logic so the native
//! `ai-memory hook` subcommand produces the same HTTP request the shell
//! scripts do: extract cwd from the payload, walk up for a
//! `.ai-memory.toml` marker, and build the query-string suffix. The two
//! request helpers are best-effort with shell-parity timeouts.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::commands::path_util::home_dir;
use ai_memory_hooks::capture_policy::MAX_MARKER_BYTES;
use ai_memory_hooks::{CaptureConfig, CapturePolicy, CaptureSource};

/// Resolve the nearest marker's capture policy without changing routing parsing.
/// Root-level marker keys are intentionally ignored here; only `[capture]` is strict.
pub fn capture_policy(cwd: &str) -> CapturePolicy {
    let home = home_dir();
    let Some(marker) = find_marker(cwd) else {
        return CapturePolicy::resolve(
            CaptureSource::Absent,
            cwd,
            home.as_deref().and_then(Path::to_str),
        );
    };
    let marker_dir = marker.parent().and_then(Path::to_str).unwrap_or(cwd);
    match read_capture_config(&marker) {
        Ok(config) => CapturePolicy::resolve(
            CaptureSource::Parsed(&config),
            marker_dir,
            home.as_deref().and_then(Path::to_str),
        ),
        Err(()) => CapturePolicy::resolve(
            CaptureSource::Invalid,
            marker_dir,
            home.as_deref().and_then(Path::to_str),
        ),
    }
}

fn read_capture_config(marker: &Path) -> Result<CaptureConfig, ()> {
    let mut bytes = Vec::with_capacity(MAX_MARKER_BYTES + 1);
    std::fs::File::open(marker)
        .map_err(|_| ())?
        .take((MAX_MARKER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > MAX_MARKER_BYTES {
        return Err(());
    }
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    let document = text.parse::<toml_edit::DocumentMut>().map_err(|_| ())?;
    let Some(capture) = document.get("capture") else {
        return Ok(CaptureConfig::default());
    };
    let table = capture.as_table().ok_or(())?;
    if table.iter().any(|(key, _)| key != "ignore_paths") {
        return Err(());
    }
    let ignore_paths = match table.get("ignore_paths") {
        None => Vec::new(),
        Some(item) => item
            .as_array()
            .ok_or(())?
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or(()))
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(CaptureConfig { ignore_paths })
}

/// First top-level `cwd` string in the payload (parity with
/// `ai_memory_extract_cwd`: take the top-level value, ignore nested
/// `cwd` fields in tool payloads).
pub fn extract_cwd(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Fixture-backed canonical routing context shared by native marker routing and
/// pre-spool capture. Only explicit agent payload shapes are accepted.
pub fn canonical_context(payload: &serde_json::Value) -> (Option<String>, Option<String>) {
    let direct = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            payload
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        })
    };
    let nested = |path: &[&str]| {
        path.iter()
            .try_fold(payload, |value, key| value.get(*key))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    };
    let cwd = direct(&["cwd", "current_dir", "working_dir", "directory"])
        .or_else(|| {
            payload
                .get("workspacePaths")
                .and_then(serde_json::Value::as_array)
                .and_then(|paths| paths.first())
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            [
                ["path", "cwd"].as_slice(),
                ["info", "directory"].as_slice(),
                ["properties", "info", "directory"].as_slice(),
                ["event", "properties", "info", "directory"].as_slice(),
                ["payload", "path", "cwd"].as_slice(),
                ["payload", "info", "directory"].as_slice(),
                ["payload", "properties", "info", "directory"].as_slice(),
            ]
            .iter()
            .find_map(|path| nested(path))
        });
    let session = direct(&[
        "session_id",
        "sessionId",
        "sessionID",
        "session",
        "conversationId",
    ])
    .or_else(|| {
        [
            ["info", "id"].as_slice(),
            ["properties", "sessionID"].as_slice(),
            ["properties", "info", "id"].as_slice(),
            ["event", "properties", "sessionID"].as_slice(),
            ["event", "properties", "info", "id"].as_slice(),
            ["payload", "info", "id"].as_slice(),
            ["payload", "properties", "sessionID"].as_slice(),
            ["payload", "properties", "info", "id"].as_slice(),
        ]
        .iter()
        .find_map(|path| nested(path))
    });
    (cwd, session)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

/// Resolve the cwd for hook bridges whose native payload may omit it.
///
/// Ordered fallback:
///
/// 1. `cwd` in the payload, if present.
/// 2. `DEVIN_PROJECT_DIR`, when the launcher provides it.
/// 3. The native hook process current directory.
pub fn resolve_cwd_with_fallbacks(
    payload: &serde_json::Value,
    mut env_lookup: impl FnMut(&str) -> Option<String>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<String> {
    non_empty(canonical_context(payload).0)
        .or_else(|| non_empty(env_lookup("DEVIN_PROJECT_DIR")))
        .or_else(|| {
            current_dir().and_then(|path| {
                let cwd = path.to_string_lossy().into_owned();
                non_empty(Some(cwd))
            })
        })
}

/// Percent-encode everything outside the RFC 3986 unreserved set
/// (`A-Z a-z 0-9 - _ . ~`), byte-wise, so multibyte UTF-8 is encoded
/// per byte. Parity with `ai_memory_url_encode` in `hooks/_lib.sh`.
///
/// An allow-list on purpose: the old deny-list missed `\` (and friends),
/// so a Windows cwd like `C:\dev\myproject` went into the query string
/// raw and the HTTP layer refused the request — the session-start hook
/// printed `{}` and the pending handoff was never fetched (#188).
/// Over-encoding is always safe; the server percent-decodes uniformly.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            other => {
                out.push_str(&format!("%{other:02X}"));
            }
        }
    }
    out
}

/// Build `&cwd=…[&workspace=…&project=…&project_strategy=…]`, mirroring
/// `ai_memory_marker_qs`: always include cwd; append marker-declared
/// fields when a `.ai-memory.toml` is found walking up toward $HOME.
///
/// `default_strategy` is the install-time default baked into the native hook
/// command by `install-hooks --project-strategy` (passed via the `hook
/// --project-strategy` flag). It fills `project_strategy` only when no marker
/// pinned one — a marker's explicit `project` / `project_strategy` always win
/// (§3.3). repo-root is resolved here, host-side, because a containerized
/// server cannot see this checkout.
pub fn marker_query_suffix(cwd: &str, default_strategy: Option<&str>) -> String {
    marker_query_suffix_impl(cwd, default_strategy, true)
}

/// [`marker_query_suffix`] without the `[briefing]` pair
/// (`&briefing=…&briefing_budget=…`).
///
/// Used by the kimi user-prompt handoff fetch: kimi discards SessionStart
/// hook stdout, so the compiled project brief is delivered on the FIRST user
/// prompt of a session (parity with Claude's once-per-SessionStart brief) and
/// later prompts re-fetch the handoff — kept on every prompt because it is
/// cheap and self-limiting (empty body when nothing is pending) — without the
/// briefing params, so the server does not recompose the brief per prompt.
pub fn marker_query_suffix_without_briefing(cwd: &str, default_strategy: Option<&str>) -> String {
    marker_query_suffix_impl(cwd, default_strategy, false)
}

/// Whether the nearest marker explicitly enables the compiled project brief.
///
/// Kimi Code uses this before creating its local once-per-session marker so
/// repositories that did not opt in do not accumulate marker files.
pub fn marker_requests_briefing(cwd: &str) -> bool {
    find_marker(cwd)
        .and_then(|marker| parse_toml_flag(&marker, "inject_on_session_start"))
        .is_some_and(|value| is_truthy(&value))
}

fn marker_query_suffix_impl(
    cwd: &str,
    default_strategy: Option<&str>,
    include_briefing: bool,
) -> String {
    let mut qs = format!("&cwd={}", url_encode(cwd));
    let (mut workspace, mut project, mut strategy, mut drop_subagent, mut default_global) =
        (None, None, None, None, None);
    let (mut briefing, mut briefing_budget) = (None, None);
    if let Some(marker) = find_marker(cwd) {
        workspace = parse_toml_key(&marker, "workspace");
        project = parse_toml_key(&marker, "project");
        strategy = parse_toml_key(&marker, "project_strategy");
        drop_subagent = parse_toml_key(&marker, "drop_subagent_captures");
        // `[recall] default_global = true` (or top-level; quoted or bare) —
        // a meta-repo opts every default-scoped read into a global search.
        default_global = parse_toml_flag(&marker, "default_global");
        // `[briefing] inject_on_session_start = true` + optional
        // `max_chars = N` — opt this repo into the compiled project brief
        // appended to the session-start handoff fetch (#176).
        briefing = parse_toml_flag(&marker, "inject_on_session_start");
        briefing_budget = parse_toml_flag(&marker, "max_chars");
    }
    if strategy.is_none() {
        strategy = default_strategy.map(str::to_owned);
    }
    if project.is_none() && matches!(strategy.as_deref(), Some("repo-root" | "repo_root")) {
        project = repo_root_project(cwd);
    }
    if let Some(val) = workspace {
        qs.push_str(&format!("&workspace={}", url_encode(&val)));
    }
    if let Some(val) = project {
        qs.push_str(&format!("&project={}", url_encode(&val)));
    }
    if let Some(val) = strategy {
        qs.push_str(&format!("&project_strategy={}", url_encode(&val)));
    }
    // Per-project `drop_subagent_captures` opt-in: forward the marker's value as
    // the `drop_subagent` flag so the server scopes the drop to this project.
    // The server interprets truthiness (`1`/`true`/…).
    if let Some(val) = drop_subagent.filter(|v| !v.is_empty()) {
        qs.push_str(&format!("&drop_subagent={}", url_encode(&val)));
    }
    // Per-repo `default_global` opt-in: forward the marker's value so the
    // server can publish it on the ActiveProject and make default-scoped read
    // tools search globally. Truthiness is decided server-side.
    if let Some(val) = default_global.filter(|v| !v.is_empty()) {
        qs.push_str(&format!("&default_global={}", url_encode(&val)));
    }
    // Per-repo session-start brief opt-in: forwarded on every request for
    // simplicity (the capture path ignores it); only the `/handoff` GET at
    // session start acts on it. Truthiness and the char-budget clamp are
    // decided server-side. Callers that deliver the brief once per session
    // (the kimi user-prompt path) pass `include_briefing = false` after the
    // first delivery so the server stops recomposing the brief per request.
    if include_briefing {
        if let Some(val) = briefing.filter(|v| !v.is_empty()) {
            qs.push_str(&format!("&briefing={}", url_encode(&val)));
        }
        if let Some(val) = briefing_budget.filter(|v| !v.is_empty()) {
            qs.push_str(&format!("&briefing_budget={}", url_encode(&val)));
        }
    }
    qs
}

/// Parse a root-level `key = <value>` line, accepting a quoted string
/// (`key = "true"`) OR a bare token (`key = true` / `key = 1`), so a
/// `[recall] default_global = true` marker works whether or not the operator
/// quotes the value. Line-based like [`parse_toml_key`], so section headers
/// are ignored; strips an optional trailing `# comment`.
fn parse_toml_flag(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let val = rest
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn repo_root_project(cwd: &str) -> Option<String> {
    let root = ai_memory_consolidate::discover_main_repo_root(Path::new(cwd)).ok()?;
    root.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Walk up from `cwd` toward `$HOME` (or the filesystem root) looking
/// for `.ai-memory.toml`. Stops at `$HOME` to avoid leaking a parent
/// user's declaration on shared machines (parity with
/// `ai_memory_find_marker`).
fn find_marker(cwd: &str) -> Option<PathBuf> {
    let home = home_dir();
    let mut dir = Path::new(cwd);
    loop {
        let candidate = dir.join(".ai-memory.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if home.as_deref() == Some(dir) {
            return None;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

/// Parse a root-level `key = "value"` line (no nesting, arrays, or
/// tables), mirroring `ai_memory_parse_toml_key`. Returns the first
/// match. Avoids pulling in a TOML parser dependency.
fn parse_toml_key(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Build a reqwest client for the hook's one-shot requests. `no_proxy`
/// skips Windows proxy auto-detection (registry / WinINET lookups), which
/// is pure overhead for a loopback/LAN POST. Built once per invocation and
/// reused for both the event POST and the handoff GET. Default root certs
/// are kept so HTTPS targets (e.g. a TLS proxy) still work.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Outcome of one spooled-event POST — enough for the drain loop to decide
/// whether a miss should cost the entry a retry attempt. Never errors, so a
/// hook/flush never fails the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOutcome {
    /// Server acknowledged with a 2xx (the engine answers `202 queued`): the
    /// entry was delivered and can be removed from the spool.
    Delivered,
    /// Server answered `429 Too Many Requests` (`hook queue full`): transient
    /// backpressure, the event was never processed. Keep it queued WITHOUT
    /// bumping attempts so saturation never burns the entry's retry budget.
    Saturated,
    /// Any other non-2xx, or a transport error: a genuine miss that should
    /// count against `MAX_ATTEMPTS`.
    Failed,
}

/// POST the payload as JSON, best-effort. `timeout` is caller-chosen: the
/// per-tool-call hot path no longer POSTs at all (it spools); the drain calls
/// this with a budget that tolerates a remote/slow server. Returns a
/// [`PostOutcome`] (never errors) so the drain can give a 429 (saturation) a
/// free retry while still bounding genuine failures by `MAX_ATTEMPTS`.
pub async fn post_hook(
    client: &reqwest::Client,
    url: &str,
    body: &str,
    token: Option<&str>,
    timeout: Duration,
) -> PostOutcome {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(body.to_owned());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => PostOutcome::Delivered,
        Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            PostOutcome::Saturated
        }
        Ok(_) => PostOutcome::Failed,
        Err(_) => PostOutcome::Failed,
    }
}

/// Outcome of one `POST /hook/batch` request — many spooled events delivered in
/// a single round-trip, so a draining client amortizes TLS + network RTT + the
/// edge auth hop over the whole batch instead of paying it per event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Server committed the leading `usize` items (contiguous prefix, oldest
    /// first). Equals the request length on full success; a smaller value means
    /// the server stopped on that item (fail-fast) — the caller deletes the
    /// prefix and charges the next item a retry.
    Accepted(usize),
    /// Server committed these item indexes, which may be non-contiguous when it
    /// skipped per-source rate-limited events and continued with later sources.
    /// If `failed_index` is present, that item failed processing and should be
    /// charged instead of assuming the first unaccepted item failed.
    AcceptedIndices {
        indices: Vec<usize>,
        failed_index: Option<usize>,
    },
    /// `429` — ingest saturated after committing this many leading items. The
    /// caller deletes that prefix and retries the rest later WITHOUT bumping
    /// attempts (saturation isn't a failure).
    Saturated(usize),
    /// `429` with a non-contiguous committed set. New servers can include this
    /// when a global saturation happens after earlier skipped items.
    SaturatedIndices(Vec<usize>),
    /// `404`/`405` — the server has no `/hook/batch` (a pre-upgrade build). The
    /// caller falls back to per-event `POST /hook` for the rest of the drain.
    Unsupported,
    /// Transport error or any other non-2xx: the batch outcome is unknown. The
    /// drain charges conservatively so trailing events that may never have been
    /// attempted do not burn retry budget.
    Failed,
}

/// POST a pre-serialized JSON array of `{url, body}` events to `<batch_url>`.
/// `bearer` authenticates the whole batch (every item shares the drain's single
/// identity). Best-effort: never errors. Reads `{"accepted": K}` from a 2xx
/// body; a 2xx whose body can't be read is treated as `Failed` (re-send rather
/// than risk dropping undelivered events).
pub async fn post_batch(
    client: &reqwest::Client,
    batch_url: &str,
    payload: &str,
    token: Option<&str>,
    timeout: Duration,
) -> BatchOutcome {
    let mut req = client
        .post(batch_url)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(payload.to_owned());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => {
                        if let Some(indices) = accepted_indices(&v) {
                            BatchOutcome::AcceptedIndices {
                                indices,
                                failed_index: failed_index(&v),
                            }
                        } else {
                            let accepted = v
                                .get("accepted")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0) as usize;
                            BatchOutcome::Accepted(accepted)
                        }
                    }
                    Err(_) => BatchOutcome::Failed,
                }
            } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let body = resp.json::<serde_json::Value>().await.ok();
                if let Some(indices) = body.as_ref().and_then(accepted_indices) {
                    BatchOutcome::SaturatedIndices(indices)
                } else {
                    let accepted = body
                        .and_then(|v| {
                            v.get("accepted")
                                .and_then(serde_json::Value::as_u64)
                                .map(|n| n as usize)
                        })
                        .unwrap_or(0);
                    BatchOutcome::Saturated(accepted)
                }
            } else if status == reqwest::StatusCode::NOT_FOUND
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            {
                BatchOutcome::Unsupported
            } else {
                BatchOutcome::Failed
            }
        }
        Err(_) => BatchOutcome::Failed,
    }
}

fn failed_index(v: &serde_json::Value) -> Option<usize> {
    v.get("failed_index")?.as_u64().map(|n| n as usize)
}

fn accepted_indices(v: &serde_json::Value) -> Option<Vec<usize>> {
    let arr = v.get("accepted_indices")?.as_array()?;
    let mut indices = Vec::with_capacity(arr.len());
    for item in arr {
        indices.push(item.as_u64()? as usize);
    }
    Some(indices)
}

/// GET the handoff text with a caller-chosen budget. Returns None on any error
/// or an empty body. This is the one synchronous read on the agent's critical
/// path (session-start injects it as context), so the budget is larger than a
/// loopback default to tolerate a remote server.
pub async fn get_handoff(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let mut req = client.get(url).timeout(timeout);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    // Warn on stderr instead of failing silently: the hook still exits 0 (a
    // hook must never break the agent), but an unreachable server would
    // otherwise be indistinguishable from "no pending handoff" (#188).
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "ai-memory hook warning: handoff fetch failed ({e}); \
                 a pending handoff (if any) was NOT injected"
            );
            return None;
        }
    };
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    if body.is_empty() { None } else { Some(body) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/handoff")
    }

    #[test]
    fn extracts_top_level_cwd() {
        let p: serde_json::Value =
            serde_json::from_str(r#"{"cwd":"/d/proj","tool_input":{"cwd":"/nested"}}"#).unwrap();
        assert_eq!(extract_cwd(&p).as_deref(), Some("/d/proj"));
    }

    #[test]
    fn missing_cwd_is_none() {
        let p: serde_json::Value = serde_json::from_str(r#"{"x":1}"#).unwrap();
        assert_eq!(extract_cwd(&p), None);
    }

    #[test]
    fn resolve_cwd_prefers_payload_over_env_and_process_cwd() {
        let p: serde_json::Value = serde_json::from_str(r#"{"cwd":"/payload"}"#).unwrap();

        let cwd = resolve_cwd_with_fallbacks(
            &p,
            |_| Some("/env".into()),
            || Some(PathBuf::from("/process")),
        );

        assert_eq!(cwd.as_deref(), Some("/payload"));
    }

    #[test]
    fn resolve_cwd_uses_devin_project_dir_when_payload_omits_cwd() {
        let p: serde_json::Value = serde_json::from_str(r#"{"source":"startup"}"#).unwrap();

        let cwd = resolve_cwd_with_fallbacks(
            &p,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "/env-project".into()),
            || Some(PathBuf::from("/process")),
        );

        assert_eq!(cwd.as_deref(), Some("/env-project"));
    }

    #[test]
    fn resolve_cwd_uses_process_cwd_when_payload_and_env_omit_cwd() {
        let p: serde_json::Value = serde_json::from_str(r#"{"source":"startup"}"#).unwrap();

        let cwd =
            resolve_cwd_with_fallbacks(&p, |_| None, || Some(PathBuf::from("process-project")));

        assert_eq!(cwd.as_deref(), Some("process-project"));
    }

    #[test]
    fn query_suffix_without_marker_has_only_cwd() {
        let qs = marker_query_suffix("/nonexistent/path/xyz", None);
        assert_eq!(qs, "&cwd=%2Fnonexistent%2Fpath%2Fxyz");
    }

    #[test]
    fn url_encode_escapes_reserved() {
        assert_eq!(url_encode("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[tokio::test]
    async fn post_hook_failed_when_server_unreachable() {
        // Port 1 is unroutable; best-effort means this resolves to `Failed`
        // (a genuine miss) rather than panicking or erroring.
        let client = build_client();
        let outcome = post_hook(
            &client,
            "http://127.0.0.1:1/hook?event=pre-tool-use",
            "{}",
            None,
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(outcome, PostOutcome::Failed);
    }

    #[tokio::test]
    async fn post_hook_saturated_on_429() {
        // 429 = server backpressure; the event was never processed, so the
        // drain must treat it as a free retry, not a failed attempt.
        let url = serve_once("429 Too Many Requests", "hook queue full").await;
        let outcome = post_hook(&build_client(), &url, "{}", None, Duration::from_secs(1)).await;
        assert_eq!(outcome, PostOutcome::Saturated);
    }

    #[tokio::test]
    async fn post_hook_delivered_on_2xx() {
        let url = serve_once("202 Accepted", "queued").await;
        let outcome = post_hook(&build_client(), &url, "{}", None, Duration::from_secs(1)).await;
        assert_eq!(outcome, PostOutcome::Delivered);
    }

    #[tokio::test]
    async fn get_handoff_ignores_non_success_status() {
        let url = serve_once("401 Unauthorized", "unauthorized").await;
        let got = get_handoff(&build_client(), &url, None, Duration::from_secs(1)).await;
        assert!(got.is_none(), "non-2xx body must not become context");
    }

    /// Happy-path TOML parser: extracts each declared root-level
    /// `key = "value"` pair. Mirrors the shell `ai_memory_parse_toml_key`.
    #[test]
    fn parse_toml_key_extracts_root_level_strings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".ai-memory.toml");
        std::fs::write(
            &marker,
            r#"
workspace = "acme"
project = "infra"
project_strategy = "repo-root"
"#,
        )
        .unwrap();
        assert_eq!(
            parse_toml_key(&marker, "workspace").as_deref(),
            Some("acme")
        );
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
        assert_eq!(
            parse_toml_key(&marker, "project_strategy").as_deref(),
            Some("repo-root")
        );
        assert_eq!(parse_toml_key(&marker, "absent"), None);
    }

    /// Shapes the naive parser deliberately doesn't handle (parity with
    /// the shell `_lib.sh` helper) — pin the contract so a future
    /// "robustify" refactor doesn't silently start matching them.
    #[test]
    fn parse_toml_key_skips_unsupported_shapes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".ai-memory.toml");
        std::fs::write(
            &marker,
            r#"
# Single-quoted values are not honoured.
workspace = 'acme'
# Comments after the value are not stripped.
project = "infra" # this is fine
"#,
        )
        .unwrap();
        assert_eq!(parse_toml_key(&marker, "workspace"), None);
        // The trailing comment is appended to the value because the parser
        // looks for the first `"` — pin it so the contract is explicit.
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
    }

    /// `find_marker` walks up from `cwd` until it finds `.ai-memory.toml`
    /// or reaches `$HOME`. Verify the walking — drop the marker two dirs
    /// above the simulated cwd and confirm it's found.
    #[test]
    fn find_marker_walks_up_from_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".ai-memory.toml");
        std::fs::write(&marker, "workspace = \"w\"\n").unwrap();
        let deep = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        let found = find_marker(deep.to_str().unwrap());
        assert_eq!(found.as_deref(), Some(marker.as_path()));
    }

    #[test]
    fn capture_section_is_strict_and_marker_read_is_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".ai-memory.toml");
        std::fs::write(
            &marker,
            "workspace = \"allowed\"\n[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        assert_eq!(
            read_capture_config(&marker).unwrap().ignore_paths,
            ["secret/**"]
        );
        std::fs::write(&marker, "[capture]\nunknown = true\n").unwrap();
        assert!(read_capture_config(&marker).is_err());
        std::fs::write(&marker, "[capture\n").unwrap();
        assert!(read_capture_config(&marker).is_err());
        std::fs::write(&marker, "x".repeat(MAX_MARKER_BYTES + 1)).unwrap();
        assert!(read_capture_config(&marker).is_err());
    }

    #[test]
    fn canonical_context_matches_supported_routing_shapes() {
        let agy =
            serde_json::json!({"workspacePaths":["/workspace/project"],"conversationId":"conv"});
        assert_eq!(
            canonical_context(&agy),
            (Some("/workspace/project".into()), Some("conv".into()))
        );
        let generated = serde_json::json!({"payload":{"properties":{"info":{"directory":"/generated"},"sessionID":"nested"}}});
        assert_eq!(
            canonical_context(&generated),
            (Some("/generated".into()), Some("nested".into()))
        );
    }

    /// `marker_query_suffix` appends `&workspace=…&project=…` (and
    /// `&project_strategy=…`, `&drop_subagent=…`) when the marker declares them.
    /// Each value is URL-encoded, so a workspace with a space round-trips as `%20`.
    /// Regression for #188: a Windows cwd must be fully percent-encoded or
    /// the HTTP layer refuses the request URL and the session-start hook
    /// silently returns `{}` while the handoff stays pending.
    #[test]
    fn url_encode_is_an_unreserved_allow_list() {
        // The reported case: raw `\` and `:` broke the request outright.
        assert_eq!(url_encode(r"C:\dev\myproject"), "C%3A%5Cdev%5Cmyproject");
        // RFC 3986 unreserved passes through untouched.
        assert_eq!(url_encode("abc-XYZ_0.9~"), "abc-XYZ_0.9~");
        // Previous deny-list behavior is preserved (space, slash, etc.).
        assert_eq!(url_encode("/home/u/my repo"), "%2Fhome%2Fu%2Fmy%20repo");
        // Multibyte UTF-8 is encoded per byte.
        assert_eq!(url_encode("r\u{e9}po"), "r%C3%A9po");
    }

    /// The full marker suffix built from a Windows cwd must parse as a real
    /// URL query — the end-to-end guarantee behind the #188 fix.
    #[test]
    fn marker_query_suffix_windows_cwd_yields_parseable_url() {
        let qs = marker_query_suffix(r"C:\dev\myproject", None);
        assert!(qs.contains("cwd=C%3A%5Cdev%5Cmyproject"), "{qs}");
        let url = format!("http://127.0.0.1:49374/handoff?agent=claude-code{qs}");
        let parsed = reqwest::Url::parse(&url).expect("must be a valid URL");
        let cwd = parsed
            .query_pairs()
            .find(|(k, _)| k == "cwd")
            .map(|(_, v)| v.into_owned())
            .expect("cwd param present");
        assert_eq!(
            cwd, r"C:\dev\myproject",
            "round-trips through percent-decoding"
        );
    }

    #[test]
    fn marker_query_suffix_appends_marker_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".ai-memory.toml");
        std::fs::write(
            &marker,
            r#"
workspace = "acme corp"
project = "infra"
project_strategy = "repo-root"
drop_subagent_captures = "true"
"#,
        )
        .unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let qs = marker_query_suffix(cwd, None);
        // cwd is encoded first; marker fields follow in the iteration order
        // of the loop in `marker_query_suffix`.
        assert!(qs.contains("&workspace=acme%20corp"), "{qs}");
        assert!(qs.contains("&project=infra"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
        assert!(qs.contains("&drop_subagent=true"), "{qs}");
    }

    /// A marker WITHOUT `drop_subagent_captures` does not forward the flag, so
    /// the server keeps that project's subagent captures (opt-in only).
    #[test]
    fn marker_query_suffix_omits_drop_subagent_when_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"acme\"\nproject = \"infra\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(!qs.contains("drop_subagent"), "{qs}");
    }

    /// A `[recall] default_global` marker (bare `true`, under a section)
    /// forwards the flag so the server can broaden default-scoped reads.
    #[test]
    fn marker_query_suffix_appends_default_global() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"acme\"\n[recall]\ndefault_global = true\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(qs.contains("&default_global=true"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_omits_default_global_when_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"acme\"\nproject = \"infra\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(!qs.contains("default_global"), "{qs}");
    }

    /// A `[briefing]` section (bare or quoted values) forwards the opt-in
    /// and the char budget so the session-start `/handoff` GET can compose
    /// the project brief (#176).
    #[test]
    fn marker_query_suffix_appends_briefing_opt_in() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"acme\"\n[briefing]\ninject_on_session_start = true\nmax_chars = 6000\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(qs.contains("&briefing=true"), "{qs}");
        assert!(qs.contains("&briefing_budget=6000"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_omits_briefing_when_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"acme\"\nproject = \"infra\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(!qs.contains("briefing"), "{qs}");
    }

    #[test]
    fn marker_requests_briefing_only_for_truthy_opt_in() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".ai-memory.toml");
        let cwd = tmp.path().to_str().unwrap();

        assert!(!marker_requests_briefing(cwd));
        std::fs::write(
            &marker,
            "[briefing]\ninject_on_session_start = false\nmax_chars = 6000\n",
        )
        .unwrap();
        assert!(!marker_requests_briefing(cwd));
        std::fs::write(
            &marker,
            "[briefing]\ninject_on_session_start = YeS\nmax_chars = 6000\n",
        )
        .unwrap();
        assert!(marker_requests_briefing(cwd));
    }

    #[test]
    fn marker_query_suffix_repo_root_non_git_keeps_project_implicit() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"oss\"\nproject_strategy = \"repo-root\"\n",
        )
        .unwrap();
        let child = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&child).unwrap();
        let qs = marker_query_suffix(child.to_str().unwrap(), None);
        assert!(qs.contains("&workspace=oss"), "{qs}");
        assert!(!qs.contains("&project="), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_repo_root_collapses_out_of_tree_worktree() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repos/acme-api");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "init",
                ])
                .status()
                .unwrap()
                .success()
        );

        let worktrees = tmp.path().join("worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        std::fs::write(
            worktrees.join(".ai-memory.toml"),
            "workspace = \"oss\"\nproject_strategy = \"repo-root\"\n",
        )
        .unwrap();
        let wt = worktrees.join("acme-api/wt-feature");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        if !std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q"])
            .arg(&wt)
            .status()
            .unwrap()
            .success()
        {
            return;
        }

        let qs = marker_query_suffix(wt.to_str().unwrap(), None);
        assert!(qs.contains("&workspace=oss"), "{qs}");
        assert!(qs.contains("&project=acme-api"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    // ── install-time default strategy (#128), no marker required ──────

    #[test]
    fn marker_query_suffix_default_repo_root_non_git_keeps_project_implicit() {
        // Baked repo-root default, no marker, not a git tree: the strategy is
        // forwarded but no project is derived (server falls back to basename).
        let tmp = tempfile::TempDir::new().unwrap();
        let child = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&child).unwrap();
        let qs = marker_query_suffix(child.to_str().unwrap(), Some("repo-root"));
        assert!(!qs.contains("&project="), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_default_repo_root_collapses_git_subdir() {
        // Baked repo-root default, no marker, inside a git subdir: the project
        // collapses to the repo-root basename.
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("contentcreator");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let sub = repo.join("transcripts");
        std::fs::create_dir_all(&sub).unwrap();
        let qs = marker_query_suffix(sub.to_str().unwrap(), Some("repo-root"));
        assert!(qs.contains("&project=contentcreator"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_marker_strategy_overrides_default() {
        // A marker that pins `project_strategy = "basename"` wins over the
        // install-time repo-root default — no repo-root derivation happens.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "project_strategy = \"basename\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), Some("repo-root"));
        assert!(qs.contains("&project_strategy=basename"), "{qs}");
        assert!(!qs.contains("repo-root"), "{qs}");
        assert!(!qs.contains("&project="), "{qs}");
    }

    #[test]
    fn marker_query_suffix_marker_project_overrides_default_repo_root() {
        // A marker's explicit `project` wins over repo-root derivation, while
        // the baked default strategy is still forwarded.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ai-memory.toml"), "project = \"pinned\"\n").unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), Some("repo-root"));
        assert!(qs.contains("&project=pinned"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }
}
