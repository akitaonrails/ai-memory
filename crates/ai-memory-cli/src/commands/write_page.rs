//! `ai-memory write-page` — write or update a wiki page via the server.
//!
//! Sends a `POST /admin/write-page` request to the running server.
//! The server handles workspace/project resolution, tier parsing,
//! frontmatter framing, and the atomic wiki write.

use std::io::Read;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::WritePageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct WritePageBody {
    workspace: String,
    project: String,
    path: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    tier: String,
    tags: Vec<String>,
    pinned: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    applies_to: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enforced_by: Option<String>,
}

#[derive(Deserialize)]
struct WritePageResponseBody {
    page_id: String,
    path: String,
    #[serde(default)]
    manifest_warning: Option<String>,
}

/// Run the `write-page` subcommand.
///
/// # Errors
/// Returns an error if an explicit scope flag is empty, `--path` is empty,
/// stdin cannot be read (when `body == "-"`), or if the POST to
/// `/admin/write-page` fails.
pub async fn run(config: &Config, args: WritePageArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    if args.path.trim().is_empty() {
        bail!(
            "{}",
            i18n.text("cli-write-page-path-was-passed-empty-specify-the-wiki-page-path-to-write")
        );
    }

    let body_text = if args.body == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context(i18n.text("cli-write-page-reading-body-from-stdin"))?;
        buf
    } else {
        args.body
    };

    // Resolve the project the same way read-page/search do: explicit flag wins,
    // otherwise derive the current project from host cwd / repo root. This keeps
    // write + read-back pairs from silently targeting different projects.
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: WritePageResponseBody = post_json(
        &endpoint,
        "/admin/write-page",
        &WritePageBody {
            workspace: workspace.clone(),
            project: project.clone(),
            path: args.path.clone(),
            body: body_text,
            title: args.title,
            kind: args.kind,
            tier: args.tier,
            tags: args.tag,
            pinned: args.pinned,
            applies_to: args.applies_to,
            enforced_by: args.enforced_by,
        },
    )
    .await
    .context(i18n.text("cli-write-page-writing-page-via-server"))?;

    if let Some(warning) = resp.manifest_warning.as_deref() {
        eprintln!(
            "{}",
            i18n.format(
                "cli-move-project-ai-memory-warning-warning",
                &[("value0", warning.to_string())]
            )
        );
    }
    println!(
        "{}",
        i18n.format(
            "cli-write-page-wrote-page-id-under",
            &[
                ("value0", resp.path.to_string()),
                ("value1", short_page_id(&resp.page_id).to_string()),
                ("value2", workspace.to_string()),
                ("value3", project.to_string())
            ]
        )
    );
    Ok(())
}

/// The last eight characters of a page id, for display. Page ids are UUIDv7,
/// whose leading characters are a timestamp that pages written close together
/// share.
pub(crate) fn short_page_id(id: &str) -> &str {
    let start = id.char_indices().rev().nth(7).map_or(0, |(i, _)| i);
    &id[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_page_id_keeps_the_random_tail() {
        assert_eq!(
            short_page_id("01a16f89-8ca9-7f40-8847-eb44e095e3ea"),
            "e095e3ea"
        );
        assert_eq!(
            short_page_id("01a16f89-eb81-70a2-8ba4-c84c5783b7fb"),
            "5783b7fb"
        );
        assert_eq!(short_page_id("abc"), "abc");
    }

    fn write_args(path: &str, project: Option<&str>) -> WritePageArgs {
        WritePageArgs {
            path: path.into(),
            body: "test body".into(),
            workspace: None,
            project: project.map(str::to_string),
            title: None,
            kind: None,
            tier: "semantic".into(),
            tag: Vec::new(),
            pinned: false,
            applies_to: Vec::new(),
            enforced_by: None,
        }
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_any_request() {
        let config = Config::default();
        let a = write_args("notes/example.md", Some("   "));
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = write_args("notes/example.md", Some("app"));
        a.workspace = Some("".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_path_is_refused_before_any_request() {
        let config = Config::default();
        let a = write_args("   ", Some("app"));
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--path was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_reading_stdin() {
        let config = Config::default();
        let mut a = write_args("notes/example.md", Some(""));
        a.body = "-".into();
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }
}
