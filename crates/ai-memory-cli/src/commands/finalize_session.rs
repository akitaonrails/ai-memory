//! `ai-memory finalize-session` — manually synthesize SessionEnd for an agent.

use ai_memory_core::{AgentKind, SessionId};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::FinalizeSessionArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerResponseError, get_json};

#[derive(Debug, Serialize)]
struct SessionEndPayload<'a> {
    session_id: String,
    cwd: &'a str,
}

#[derive(Debug, Serialize)]
struct HookBatchItem<'a> {
    url: String,
    body: SessionEndPayload<'a>,
}

#[derive(Debug, Deserialize)]
struct HookBatchAck {
    accepted: usize,
}

/// Response shape of `GET /admin/open-sessions` on the server.
#[derive(Debug, Deserialize)]
struct OpenSessionsResponse {
    sessions: Vec<OpenSessionEntry>,
}

/// One open session as reported by `GET /admin/open-sessions`.
#[derive(Debug, Deserialize)]
struct OpenSessionEntry {
    session_id: String,
    cwd: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FinalizeSessionReport {
    workspace: String,
    project: String,
    agent: String,
    finalized: Vec<String>,
}

/// Run the `finalize-session` subcommand.
///
/// # Errors
/// Returns an error if the configured server cannot list the scope's open
/// sessions or rejects a synthetic `session-end` hook.
pub async fn run(config: &Config, args: FinalizeSessionArgs) -> Result<()> {
    let agent = args.agent;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let sessions = fetch_open_sessions(
        &endpoint,
        &workspace,
        &project,
        agent,
        args.all,
        args.all_owners,
        args.session_id,
    )
    .await?;
    if sessions.is_empty() {
        return print_report(args, workspace, project, agent, Vec::new());
    }

    let client = reqwest::Client::new();
    let fallback_cwd = effective_cwd(config)?;
    let mut finalized = Vec::with_capacity(sessions.len());
    for session in &sessions {
        post_session_end_batch(
            &client,
            &endpoint,
            session,
            fallback_cwd.as_str(),
            &workspace,
            &project,
            agent,
            args.all_owners,
        )
        .await?;
        finalized.push(session.session_id.clone());
    }

    print_report(args, workspace, project, agent, finalized)
}

/// List open sessions for the scope + agent via the server. An unknown
/// workspace/project fails closed server-side with a 404; that maps to
/// "nothing to finalize" here, matching the previous direct-DB behavior
/// for a missing scope.
async fn fetch_open_sessions(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    agent: AgentKind,
    all: bool,
    all_owners: bool,
    session_id: Option<SessionId>,
) -> Result<Vec<OpenSessionEntry>> {
    let all = if all { "true" } else { "false" };
    let all_owners = if all_owners { "true" } else { "false" };
    let mut query = vec![
        ("workspace", workspace),
        ("project", project),
        ("agent", agent.as_str()),
        ("all", all),
        ("all_owners", all_owners),
    ];
    let session_id = session_id.map(|sid| sid.to_string());
    if let Some(sid) = session_id.as_deref() {
        query.push(("session_id", sid));
    }
    let result = get_json::<OpenSessionsResponse>(endpoint, "/admin/open-sessions", &query).await;
    match result {
        Ok(response) => Ok(response.sessions),
        Err(e) => {
            if let Some(server_err) = e.downcast_ref::<ServerResponseError>()
                && server_err.status() == reqwest::StatusCode::NOT_FOUND
            {
                return Ok(Vec::new());
            }
            Err(e)
        }
    }
}

fn print_report(
    args: FinalizeSessionArgs,
    workspace: String,
    project: String,
    agent: AgentKind,
    finalized: Vec<String>,
) -> Result<()> {
    let report = FinalizeSessionReport {
        workspace,
        project,
        agent: agent.as_str().to_string(),
        finalized,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.finalized.is_empty() {
        println!(
            "No open {} sessions matched {}/{}",
            report.agent, report.workspace, report.project
        );
    } else {
        println!(
            "Finalized {} {} session(s) for {}/{}",
            report.finalized.len(),
            report.agent,
            report.workspace,
            report.project
        );
        for session_id in &report.finalized {
            println!("  - {session_id}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn post_session_end_batch(
    client: &reqwest::Client,
    endpoint: &ServerEndpoint,
    session: &OpenSessionEntry,
    fallback_cwd: &str,
    workspace: &str,
    project: &str,
    agent: AgentKind,
    all_owners: bool,
) -> Result<()> {
    let cwd = session.cwd.as_deref().unwrap_or(fallback_cwd);
    let hook_url = session_end_hook_url(endpoint, cwd, workspace, project, agent, all_owners)?;
    let batch_url = endpoint.build_url("/hook/batch");
    let items = [HookBatchItem {
        url: hook_url,
        body: SessionEndPayload {
            session_id: session.session_id.clone(),
            cwd,
        },
    }];
    let request = client.post(&batch_url).json(&items);
    let request = endpoint.authenticate(request);
    let response = request
        .send()
        .await
        .with_context(|| format!("posting synthetic session-end batch to {batch_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("server returned {status}: {body}");
    }
    let ack: HookBatchAck = response
        .json()
        .await
        .with_context(|| format!("parsing hook batch ack from {batch_url}"))?;
    if ack.accepted != 1 {
        bail!(
            "server accepted {} of 1 synthetic session-end events",
            ack.accepted
        );
    }
    Ok(())
}

fn session_end_hook_url(
    endpoint: &ServerEndpoint,
    cwd: &str,
    workspace: &str,
    project: &str,
    agent: AgentKind,
    all_owners: bool,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&endpoint.build_url("/hook"))
        .context("building synthetic session-end hook URL")?;
    url.query_pairs_mut()
        .append_pair("event", "session-end")
        .append_pair("agent", agent.as_str())
        .append_pair("cwd", cwd)
        .append_pair("workspace", workspace)
        .append_pair("project", project);
    if all_owners {
        url.query_pairs_mut().append_pair("all_owners", "true");
    }
    Ok(url.into())
}

fn effective_cwd(config: &Config) -> Result<String> {
    if let Some(host_cwd) = config.runtime_env.host_cwd()
        && !host_cwd.trim().is_empty()
    {
        return Ok(host_cwd.to_string());
    }
    Ok(std::env::current_dir()
        .context("getting CWD for synthetic session-end")?
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{NewSession, SessionId};
    use ai_memory_store::Store;
    use tempfile::TempDir;

    #[tokio::test]
    async fn selects_latest_scoped_session_for_requested_agent_by_default() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let target = store
            .writer
            .get_or_create_project(ws, "target".to_string(), None)
            .await
            .unwrap();
        let other_project = store
            .writer
            .get_or_create_project(ws, "other".to_string(), None)
            .await
            .unwrap();
        let older = SessionId::new();
        let latest = SessionId::new();
        let other_agent = SessionId::new();
        let other_scope = SessionId::new();
        for (id, project_id, agent) in [
            (older, target, AgentKind::AntigravityCli),
            (other_agent, target, AgentKind::Codex),
            (other_scope, other_project, AgentKind::AntigravityCli),
            (latest, target, AgentKind::AntigravityCli),
        ] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: agent,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                    actor_user: None,
                })
                .await
                .unwrap();
        }

        let selected = store
            .reader
            .open_sessions_for_scope_agent(
                ws,
                target,
                AgentKind::AntigravityCli,
                ai_memory_core::OwnerFilter::Any,
                Some(1),
            )
            .await
            .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].session_id, latest);
    }

    /// Regression for the race a blind wrapper hits when several terminal
    /// tabs each run Kiro CLI against the same repo: two sessions for the
    /// same agent+scope are open at once (`older` started first, `latest`
    /// started after — e.g. opened in a second tab while the first was
    /// still running). Targeting `older` by its exact id must return only
    /// `older`, never falling back to "the newest open one" and closing
    /// out the still-active `latest` session instead.
    #[tokio::test]
    async fn exact_session_id_targets_that_session_even_with_a_newer_one_open() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "target".to_string(), None)
            .await
            .unwrap();
        let older = SessionId::new();
        let latest = SessionId::new();
        for id in [older, latest] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::KiroCli,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                    actor_user: None,
                })
                .await
                .unwrap();
        }

        let selected = store
            .reader
            .open_session_for_scope_agent_by_id(
                ws,
                proj,
                AgentKind::KiroCli,
                ai_memory_core::OwnerFilter::Any,
                older,
            )
            .await
            .unwrap();

        assert_eq!(
            selected.map(|session| session.session_id),
            Some(older),
            "must target the requested session, not the newest open one"
        );

        // The other, still-open session must be untouched and independently
        // discoverable — proving the exact-id filter didn't just narrow the
        // default query, it left the sibling session alone.
        let still_open = store
            .reader
            .open_session_for_scope_agent_by_id(
                ws,
                proj,
                AgentKind::KiroCli,
                ai_memory_core::OwnerFilter::Any,
                latest,
            )
            .await
            .unwrap();
        assert_eq!(still_open.map(|session| session.session_id), Some(latest));
    }

    #[test]
    fn synthetic_end_url_propagates_all_owners_once_when_requested() {
        let endpoint =
            ServerEndpoint::from_pair(Some("http://127.0.0.1:49374/base".to_string()), None);
        let url = session_end_hook_url(
            &endpoint,
            "/tmp/project",
            "default",
            "project",
            AgentKind::AntigravityCli,
            true,
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query: Vec<_> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.path(), "/base/hook");
        assert_eq!(
            query,
            vec![
                ("event".to_string(), "session-end".to_string()),
                ("agent".to_string(), "antigravity-cli".to_string()),
                ("cwd".to_string(), "/tmp/project".to_string()),
                ("workspace".to_string(), "default".to_string()),
                ("project".to_string(), "project".to_string()),
                ("all_owners".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn synthetic_end_url_omits_all_owners_when_not_requested() {
        let endpoint =
            ServerEndpoint::from_pair(Some("http://127.0.0.1:49374/base".to_string()), None);
        let url = session_end_hook_url(
            &endpoint,
            "/tmp/project",
            "default",
            "project",
            AgentKind::AntigravityCli,
            false,
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query: Vec<_> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.path(), "/base/hook");
        assert_eq!(
            query,
            vec![
                ("event".to_string(), "session-end".to_string()),
                ("agent".to_string(), "antigravity-cli".to_string()),
                ("cwd".to_string(), "/tmp/project".to_string()),
                ("workspace".to_string(), "default".to_string()),
                ("project".to_string(), "project".to_string()),
            ]
        );
    }
}
