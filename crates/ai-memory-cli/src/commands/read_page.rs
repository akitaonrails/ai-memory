//! `ai-memory read-page` — display the full body of a wiki page.
//!
//! Two resolution modes:
//! - `--path <path>`: fetch the page at that exact wiki path.
//! - positional `<query>`: FTS5 search scoped to the current project;
//!   fetches the top-ranking hit's full body.
//!
//! The project is auto-detected from the current directory (same heuristic
//! as `search`). Pass `--project` to target a different project explicitly.
//!
//! Thin HTTP client. Never opens the store or wiki directly.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::ReadPageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Mirrors the `ReadPageResponse` struct on the server side.
#[derive(Debug, Deserialize, Serialize)]
struct PageContent {
    path: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    project: String,
    title: Option<String>,
    body: String,
    frontmatter: serde_json::Value,
    /// Set by the server when the page's `expires_at` TTL has passed: the
    /// exact-path read still answers, while `search` no longer lists the page.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    expired: bool,
}

/// Run the `read-page` subcommand.
///
/// # Errors
/// Returns an error if an explicit scope flag, `--path`, or the query is empty;
/// the server is unreachable; returns non-2xx; or neither `--path` nor a query
/// is supplied.
pub async fn run(config: &Config, args: ReadPageArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    let path = match args.path.as_deref() {
        Some(p) if p.trim().is_empty() => {
            bail!(
                "{}",
                i18n.text("cli-read-page-path-was-passed-empty-specify-the-wiki-page-path-to-read")
            );
        }
        Some(p) => Some(p),
        None => None,
    };
    let query = match args.query.as_deref() {
        Some(q) if q.trim().is_empty() => {
            bail!(
                "{}",
                i18n.text(
                    "cli-read-page-search-query-was-passed-empty-specify-a-query-or-path-wiki-path"
                )
            );
        }
        Some(q) => Some(q),
        None => None,
    };
    if path.is_none() && query.is_none() {
        bail!(
            "{}",
            i18n.text("cli-read-page-provide-a-query-positional-or-path-wiki-path")
        );
    }

    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;

    let page: PageContent = if let Some(raw_path) = path {
        get_json(
            &ep,
            "/admin/read-page",
            &[
                ("workspace", workspace.as_str()),
                ("project", project.as_str()),
                ("path", raw_path),
            ],
        )
        .await?
    } else if let Some(q) = query {
        get_json(
            &ep,
            "/admin/read-page",
            &[
                ("workspace", workspace.as_str()),
                ("project", project.as_str()),
                ("q", q),
            ],
        )
        .await?
    } else {
        unreachable!();
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else {
        print!("{}", render_text(&config.i18n, &page));
    }
    Ok(())
}

fn render_text(i18n: &ai_memory_core::i18n::Localizer, page: &PageContent) -> String {
    let mut out = String::new();
    if let Some(title) = &page.title {
        out.push_str(&format!("# {title}\n"));
    }
    out.push_str(&i18n.format("cli-read-page-path", &[("value0", page.path.to_string())]));
    if page.expired {
        out.push_str(&i18n.text("cli-read-page-expired-true"));
    }
    out.push('\n');
    out.push_str(&page.body);
    if !page.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(expired: bool) -> PageContent {
        PageContent {
            path: "notes/example.md".into(),
            workspace: "default".into(),
            project: "app".into(),
            title: Some("Example".into()),
            body: "body".into(),
            frontmatter: serde_json::json!({}),
            expired,
        }
    }

    #[test]
    fn text_output_marks_only_expired_pages() {
        assert_eq!(
            render_text(&ai_memory_core::i18n::Localizer::default(), &page(false)),
            "# Example\npath: notes/example.md\n\nbody\n"
        );
        assert_eq!(
            render_text(&ai_memory_core::i18n::Localizer::default(), &page(true)),
            "# Example\npath: notes/example.md\nexpired: true\n\nbody\n"
        );
    }

    #[test]
    fn older_server_response_defaults_to_live() {
        let parsed: PageContent = serde_json::from_value(serde_json::json!({
            "path": "notes/example.md",
            "title": null,
            "body": "body\n",
            "frontmatter": {}
        }))
        .unwrap();
        assert!(!parsed.expired);
    }

    fn args(
        path: Option<&str>,
        query: Option<&str>,
        workspace: Option<&str>,
        project: Option<&str>,
    ) -> ReadPageArgs {
        ReadPageArgs {
            query: query.map(Into::into),
            path: path.map(Into::into),
            workspace: workspace.map(Into::into),
            project: project.map(Into::into),
            json: false,
        }
    }

    #[tokio::test]
    async fn an_empty_workspace_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(
                &Config::default(),
                args(Some("notes/example.md"), None, Some(empty), None),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("--workspace was passed empty"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_project_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(
                &Config::default(),
                args(Some("notes/example.md"), None, None, Some(empty)),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("--project was passed empty"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_path_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(&Config::default(), args(Some(empty), None, None, None))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("--path was passed empty"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_query_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(&Config::default(), args(None, Some(empty), None, None))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("search query was passed empty"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn missing_both_path_and_query_is_refused_before_scope_or_network_work() {
        let error = run(&Config::default(), args(None, None, None, None))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("provide a query (positional) or --path <wiki-path>"),
            "unexpected error: {error}"
        );
    }
}
