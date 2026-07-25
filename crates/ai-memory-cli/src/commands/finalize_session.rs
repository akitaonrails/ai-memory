//! `ai-memory finalize-session` — manually synthesize SessionEnd for Codex.

use ai_memory_core::AgentKind;
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
    let agent = args.agent.kind();
    let project = super::resolve_project_name(config, args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let sessions =
        fetch_open_sessions(&endpoint, &args.workspace, &project, agent, args.all).await?;
    if sessions.is_empty() {
        return print_report(args, project, agent, Vec::new());
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
            &args.workspace,
            &project,
            agent,
        )
        .await?;
        finalized.push(session.session_id.clone());
    }

    print_report(args, project, agent, finalized)
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
) -> Result<Vec<OpenSessionEntry>> {
    let all = if all { "true" } else { "false" };
    let result = get_json::<OpenSessionsResponse>(
        endpoint,
        "/admin/open-sessions",
        &[
            ("workspace", workspace),
            ("project", project),
            ("agent", agent.as_str()),
            ("all", all),
        ],
    )
    .await;
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
    project: String,
    agent: AgentKind,
    finalized: Vec<String>,
) -> Result<()> {
    let report = FinalizeSessionReport {
        workspace: args.workspace,
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

async fn post_session_end_batch(
    client: &reqwest::Client,
    endpoint: &ServerEndpoint,
    session: &OpenSessionEntry,
    fallback_cwd: &str,
    workspace: &str,
    project: &str,
    agent: AgentKind,
) -> Result<()> {
    let cwd = session.cwd.as_deref().unwrap_or(fallback_cwd);
    let hook_url = session_end_hook_url(endpoint, cwd, workspace, project, agent)?;
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
) -> Result<String> {
    let mut url = reqwest::Url::parse(&endpoint.build_url("/hook"))
        .context("building synthetic session-end hook URL")?;
    url.query_pairs_mut()
        .append_pair("event", "session-end")
        .append_pair("agent", agent.as_str())
        .append_pair("cwd", cwd)
        .append_pair("workspace", workspace)
        .append_pair("project", project);
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
    async fn selects_latest_scoped_codex_session_only_by_default() {
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
            (older, target, AgentKind::Codex),
            (other_agent, target, AgentKind::ClaudeCode),
            (other_scope, other_project, AgentKind::Codex),
            (latest, target, AgentKind::Codex),
        ] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: agent,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                })
                .await
                .unwrap();
        }

        let selected = store
            .reader
            .open_sessions_for_scope_agent(ws, target, AgentKind::Codex, Some(1))
            .await
            .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].session_id, latest);
    }
}
