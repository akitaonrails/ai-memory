//! `ai-memory processing-jobs` — start and inspect durable chunked
//! session-processing jobs over the root-only `/admin/processing-jobs` API.
//!
//! Thin HTTP client: the server owns every gate. A server with
//! `[chunking].enabled = false` refuses `start` with an explicit 404, which
//! this command reports as a disabled feature (non-zero exit) instead of
//! pretending success.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;

use crate::cli::{
    ProcessingJobsArgs, ProcessingJobsCommand, ProcessingJobsStartArgs, ProcessingJobsStatusArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerResponseError, get_json, post_json};

/// Request sent to `POST /admin/processing-jobs`.
#[derive(Serialize)]
struct StartRequest<'a> {
    workspace: &'a str,
    project: &'a str,
    session_id: &'a str,
    kind: &'a str,
}

/// Run the `processing-jobs` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable, rejects the request, or
/// reports the feature as disabled.
pub async fn run(config: &Config, args: ProcessingJobsArgs) -> Result<()> {
    match args.command {
        ProcessingJobsCommand::Start(args) => start(config, args).await,
        ProcessingJobsCommand::Status(args) => status(config, args).await,
    }
}

async fn start(config: &Config, args: ProcessingJobsStartArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let request = StartRequest {
        workspace: &workspace,
        project: &project,
        session_id: &args.session_id,
        kind: &args.kind,
    };
    let response: Value = post_json(&endpoint, "/admin/processing-jobs", &request)
        .await
        .map_err(disabled_or_original)?;
    print_status(&response, args.json)
}

async fn status(config: &Config, args: ProcessingJobsStatusArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let path = format!("/admin/processing-jobs/{}", args.job_id);
    let response: Value = get_json(
        &endpoint,
        &path,
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    print_status(&response, args.json)
}

/// A 404 whose body says the feature is off becomes an explicit disabled
/// error; every other failure passes through untouched.
fn disabled_or_original(error: anyhow::Error) -> anyhow::Error {
    let disabled = error
        .downcast_ref::<ServerResponseError>()
        .is_some_and(|e| {
            e.status() == reqwest::StatusCode::NOT_FOUND && e.body().contains("disabled")
        });
    if disabled {
        anyhow!(
            "processing jobs are disabled on this server; set [chunking].enabled = true and restart it"
        )
    } else {
        error
    }
}

fn print_status(status: &Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        println!("{}", render_human(status)?);
    }
    Ok(())
}

fn render_human(status: &Value) -> Result<String> {
    let field = |name: &str| status.get(name).and_then(Value::as_str).unwrap_or("-");
    let mut lines = vec![
        format!("job {}", field("job_id")),
        format!("  kind:    {}", field("kind")),
        format!("  session: {}", field("session_id")),
        format!("  state:   {}", field("state")),
    ];
    if let Some(calls) = status.get("calls_used").and_then(Value::as_u64) {
        let max = status
            .get("max_calls")
            .and_then(Value::as_u64)
            .map_or_else(|| "?".to_string(), |m| m.to_string());
        lines.push(format!("  calls:   {calls}/{max}"));
    }
    if let (Some(wait), Some(exec)) = (
        status.get("wait_ms").and_then(Value::as_u64),
        status.get("exec_ms").and_then(Value::as_u64),
    ) {
        lines.push(format!("  time:    wait {wait} ms, exec {exec} ms"));
    }
    if let Some(error) = status.get("last_error").and_then(Value::as_str) {
        lines.push(format!("  error:   {error}"));
    }
    if let Some(result) = status.get("result").filter(|r| !r.is_null()) {
        lines.push(format!(
            "  result:\n{}",
            serde_json::to_string_pretty(result).context("rendering job result")?
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
        crate::http_client::server_response_error_for_test(
            reqwest::Method::POST,
            "/admin/processing-jobs",
            status,
            body.to_string(),
        )
    }

    #[test]
    fn disabled_404_is_reported_as_disabled() {
        let err = disabled_or_original(http_error(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"error":"processing jobs are disabled"}"#,
        ));
        assert!(err.to_string().contains("disabled on this server"));
    }

    #[test]
    fn other_failures_pass_through() {
        let err = disabled_or_original(http_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":"admin required"}"#,
        ));
        assert!(err.to_string().contains("403"), "{err}");
        // A 404 that is not the disabled marker (unknown job/scope) is not
        // rewritten either.
        let err = disabled_or_original(http_error(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"error":"project not found"}"#,
        ));
        assert!(err.to_string().contains("project not found"), "{err}");
    }
}
