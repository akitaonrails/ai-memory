//! `ai-memory list-projects` — plain read-only listing of every
//! workspace/project pair the server knows about.
//!
//! Before this command, the only way to see this from the client was to hit
//! `GET /api/v1/projects` directly with curl/Invoke-RestMethod. `show`'s own
//! use of that endpoint is folded into an interactive picker and silently
//! falls back to client-local scan results when the server call fails, so it
//! was never a plain, scriptable listing on its own.

use anyhow::Result;

use crate::cli::ListProjectsArgs;
use crate::commands::show::ProjectRow;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

pub async fn run(config: &Config, args: ListProjectsArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let rows = fetch_sorted(&endpoint, args.workspace.as_deref()).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    print!("{}", render_human(&rows));
    Ok(())
}

async fn fetch_sorted(
    endpoint: &ServerEndpoint,
    workspace: Option<&str>,
) -> Result<Vec<ProjectRow>> {
    let query = workspace
        .filter(|value| !value.is_empty())
        .map_or_else(Vec::new, |workspace| vec![("workspace", workspace)]);
    let mut rows: Vec<ProjectRow> = get_json(endpoint, "/api/v1/projects", &query).await?;
    rows.sort_by(|a, b| {
        a.workspace_name
            .cmp(&b.workspace_name)
            .then_with(|| a.project_name.cmp(&b.project_name))
    });
    Ok(rows)
}

fn render_human(rows: &[ProjectRow]) -> String {
    if rows.is_empty() {
        return "No projects found.\n".to_owned();
    }

    let workspace_width = rows
        .iter()
        .map(|row| row.workspace_name.len())
        .max()
        .unwrap_or(0)
        .max("WORKSPACE".len());
    let project_width = rows
        .iter()
        .map(|row| row.project_name.len())
        .max()
        .unwrap_or(0)
        .max("PROJECT".len());

    let mut out = format!(
        "{:workspace_width$}  {:project_width$}  {:>6}  LAST UPDATED\n",
        "WORKSPACE", "PROJECT", "PAGES"
    );
    for row in rows {
        out += &format!(
            "{:workspace_width$}  {:project_width$}  {:>6}  {}\n",
            row.workspace_name,
            row.project_name,
            row.page_count,
            row.last_updated.as_deref().unwrap_or("-")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(workspace: &str, project: &str, pages: u64, last_updated: Option<&str>) -> ProjectRow {
        ProjectRow {
            workspace_name: workspace.to_owned(),
            project_name: project.to_owned(),
            page_count: pages,
            last_updated: last_updated.map(str::to_owned),
        }
    }

    #[test]
    fn render_human_reports_an_empty_server() {
        assert_eq!(render_human(&[]), "No projects found.\n");
    }

    #[test]
    fn render_human_prints_a_header_and_one_row_per_project() {
        let rows = vec![row("default", "acme-api", 12, Some("2026-09-30T00:00:00Z"))];
        let out = render_human(&rows);
        assert!(out.starts_with("WORKSPACE"), "{out}");
        assert!(out.contains("default"), "{out}");
        assert!(out.contains("acme-api"), "{out}");
        assert!(out.contains("12"), "{out}");
        assert!(out.contains("2026-09-30T00:00:00Z"), "{out}");
    }

    #[test]
    fn render_human_shows_a_dash_for_a_project_with_no_pages_yet() {
        let rows = vec![row("default", "empty-project", 0, None)];
        let out = render_human(&rows);
        let data_line = out.lines().nth(1).expect("one data line");
        assert!(data_line.contains("empty-project"), "{data_line}");
        assert!(data_line.trim_end().ends_with('-'), "{data_line}");
    }

    #[test]
    fn render_human_columns_widen_to_the_longest_name() {
        let rows = vec![
            row("default", "a", 1, None),
            row(
                "a-much-longer-workspace-name",
                "a-much-longer-project-name",
                2,
                None,
            ),
        ];
        let out = render_human(&rows);
        // Every data line must be at least as wide as the longest name in its
        // column, i.e. the short names are padded, not truncated.
        for line in out.lines().skip(1) {
            assert!(
                line.len() >= "a-much-longer-workspace-name".len(),
                "line not padded to the widest workspace name: {line:?}"
            );
        }
    }

    #[test]
    fn sort_orders_by_workspace_then_project() {
        let mut rows = [
            row("default", "zeta", 1, None),
            row("alpha", "zeta", 1, None),
            row("default", "alpha", 1, None),
        ];
        rows.sort_by(|a, b| {
            a.workspace_name
                .cmp(&b.workspace_name)
                .then_with(|| a.project_name.cmp(&b.project_name))
        });
        let ordered: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.workspace_name.as_str(), r.project_name.as_str()))
            .collect();
        assert_eq!(
            ordered,
            vec![("alpha", "zeta"), ("default", "alpha"), ("default", "zeta")]
        );
    }
}
