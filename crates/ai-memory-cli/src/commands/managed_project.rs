//! Explicit recovery from ambiguous managed project coordinates.

use std::io::{self, BufRead, Write};

use ai_memory_core::{
    ManagedProjectCandidatesRequest, ManagedProjectCandidatesResponse, ManagedProjectScope,
};
use anyhow::{Context as _, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::http_client::{ServerEndpoint, ServerResponseError, post_json};

pub(super) fn validate_prepared_scope(
    request: &ai_memory_core::PrepareManagedRunRequest,
    response: &ai_memory_core::PrepareManagedRunResponse,
) -> Result<()> {
    if request.project_id.is_some() && response.resolved_project.is_none() {
        bail!(
            "the server did not confirm project-ID selection; upgrade the server before launching"
        );
    }
    if let Some(scope) = &response.resolved_project
        && (scope.workspace != request.workspace.trim()
            || request.project_id.is_some_and(|id| id != scope.project_id))
    {
        bail!("the server returned a different managed project scope; refusing to launch");
    }
    Ok(())
}

pub(super) fn is_ambiguous(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServerResponseError>()
        .is_some_and(|error| {
            error.status() == reqwest::StatusCode::BAD_REQUEST
                && serde_json::from_str::<serde_json::Value>(error.body())
                    .ok()
                    .and_then(|body| {
                        body.get("code")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some("ambiguous_project")
        })
}

pub(super) fn is_missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServerResponseError>()
        .is_some_and(|error| error.status() == reqwest::StatusCode::NOT_FOUND)
}

pub(super) async fn choose(
    i18n: &ai_memory_core::i18n::Localizer,
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    interactive: bool,
    interrupted: &CancellationToken,
) -> Result<ManagedProjectScope> {
    let response: ManagedProjectCandidatesResponse = post_json(
        endpoint,
        "/workstream/project-candidates",
        &ManagedProjectCandidatesRequest {
            workspace: workspace.to_owned(),
            project: project.to_owned(),
        },
    )
    .await
    .context(
        "listing authorized project candidates; upgrade the server if this endpoint is unavailable",
    )?;
    if response.candidates.is_empty() {
        bail!(
            "no writable project candidates are available; ask the operator for an authorized project ID and use --project-id <id>"
        );
    }
    if !interactive {
        let choices = response
            .candidates
            .iter()
            .map(|row| {
                format!(
                    "{} ({})",
                    row.project_id,
                    super::show::terminal_text(&row.project)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "project name is ambiguous; rerun with --project-id <id>. Authorized choices: {choices}"
        );
    }
    let chooser_i18n = i18n.clone();
    let read = tokio::task::spawn_blocking(move || {
        read_choice(
            &chooser_i18n,
            &response,
            &mut io::stdin().lock(),
            &mut io::stderr(),
        )
    });
    tokio::select! {
        () = interrupted.cancelled() => bail!("project selection interrupted; no managed run was opened"),
        result = read => result.context("reading project selection")?,
    }
}

fn read_choice(
    i18n: &ai_memory_core::i18n::Localizer,
    response: &ManagedProjectCandidatesResponse,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<ManagedProjectScope> {
    writeln!(
        output,
        "{}",
        i18n.diagnostic(
            "This checkout's project name is ambiguous. Select the existing project to use:"
        )
    )?;
    for (index, scope) in response.candidates.iter().enumerate() {
        writeln!(
            output,
            "  {}. {}/{}  [{}]",
            index + 1,
            super::show::terminal_text(&scope.workspace),
            super::show::terminal_text(&scope.project),
            scope.project_id
        )?;
    }
    if response.truncated {
        writeln!(
            output,
            "{}",
            i18n.diagnostic("More matches exist; use --project-id if your project is not shown.")
        )?;
    }
    write!(
        output,
        "{}",
        i18n.diagnostic("Select a number, or Enter to cancel: ")
    )?;
    output.flush()?;
    let mut line = String::new();
    std::io::Read::take(&mut *input, 128).read_line(&mut line)?;
    let index = line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1));
    match index.and_then(|index| response.candidates.get(index)) {
        Some(scope) => Ok(scope.clone()),
        None => bail!("project selection cancelled; no managed run was opened"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn project_id_noninteractive_ambiguity_is_actionable_without_selecting() {
        let scope = ManagedProjectScope {
            workspace_id: ai_memory_core::WorkspaceId::new(),
            project_id: ai_memory_core::ProjectId::new(),
            workspace: "default".into(),
            project: "acme-api".into(),
        };
        let response = ManagedProjectCandidatesResponse {
            candidates: vec![scope.clone()],
            truncated: false,
        };
        let app = axum::Router::new().route(
            "/workstream/project-candidates",
            axum::routing::post(move || {
                let response = response.clone();
                async move { axum::Json(response) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = ServerEndpoint::from_pair(Some(format!("http://{address}")), None);
        let error = choose(
            &ai_memory_core::i18n::Localizer::default(),
            &endpoint,
            "default",
            "api",
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("--project-id"));
        assert!(error.to_string().contains(&scope.project_id.to_string()));
        server.abort();
    }

    #[test]
    fn project_id_choice_requires_an_explicit_valid_number() {
        let scope = ManagedProjectScope {
            workspace_id: ai_memory_core::WorkspaceId::new(),
            project_id: ai_memory_core::ProjectId::new(),
            workspace: "default".into(),
            project: "acme-api".into(),
        };
        let response = ManagedProjectCandidatesResponse {
            candidates: vec![scope.clone()],
            truncated: false,
        };
        for input in ["", "\n", "0\n", "2\n", "q\n"] {
            assert!(
                read_choice(
                    &ai_memory_core::i18n::Localizer::default(),
                    &response,
                    &mut input.as_bytes(),
                    &mut Vec::new()
                )
                .is_err()
            );
        }
        assert_eq!(
            read_choice(
                &ai_memory_core::i18n::Localizer::default(),
                &response,
                &mut "1\n".as_bytes(),
                &mut Vec::new()
            )
            .unwrap(),
            scope
        );
    }
}
