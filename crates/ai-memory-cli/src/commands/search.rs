//! `ai-memory search` — run an FTS5 query against the wiki index.
//!
//! Thin HTTP client. Calls `GET /admin/search?q=…&limit=…`; renders
//! the hits as human text or JSON. Never opens the store directly.
//!
//! The scope comes from the cwd (or `--workspace`/`--project`) unless
//! `--global` is passed: then no scope is sent, the server searches every
//! project's wiki pages, and each hit carries its `workspace`/`project`.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::SearchArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// FTS5 hit, mirrors the `/admin/search` fields used here.
#[derive(Debug, Deserialize, Serialize)]
struct Hit {
    /// Set only on a global search: the workspace the page lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    /// Set only on a global search: the project the page lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    path: String,
    title: String,
    snippet: String,
    rank: f64,
}

/// Run the `search` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or returns non-2xx.
pub async fn run(config: &Config, args: SearchArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let limit_str = args.limit.to_string();
    let mut query = vec![("q", args.query.clone()), ("limit", limit_str)];
    if !args.global {
        let (workspace, project) =
            super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
        query.push(("workspace", workspace));
        query.push(("project", project));
    }
    let query: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let hits: Vec<Hit> = get_json(&ep, "/admin/search", &query).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        print!("{}", render_text(&config.i18n, &args.query, &hits));
    }
    Ok(())
}

fn render_text(i18n: &ai_memory_core::i18n::Localizer, query: &str, hits: &[Hit]) -> String {
    use std::fmt::Write as _;
    if hits.is_empty() {
        return i18n.format(
            "cli-search-no-results-for-query",
            &[("value0", format!("{:?}", query))],
        );
    }
    let mut out = i18n.format(
        "cli-search-result-s-for-query",
        &[
            ("value0", format!("{}", hits.len())),
            ("value1", format!("{:?}", query)),
        ],
    );
    for hit in hits {
        let _ = match (&hit.workspace, &hit.project) {
            (Some(workspace), Some(project)) => writeln!(
                out,
                "{}",
                i18n.format(
                    "cli-search-scoped-hit",
                    &[
                        ("value0", workspace.to_string()),
                        ("value1", project.to_string()),
                        ("value2", hit.path.to_string()),
                        ("value3", format!("{:.4}", hit.rank))
                    ]
                )
            ),
            _ => writeln!(
                out,
                "{}",
                i18n.format(
                    "cli-search-hit",
                    &[
                        ("value0", hit.path.to_string()),
                        ("value1", format!("{:.4}", hit.rank))
                    ]
                )
            ),
        };
        let _ = writeln!(out, "    {}", hit.title);
        let _ = writeln!(out, "    {}", hit.snippet);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(json: serde_json::Value) -> Hit {
        serde_json::from_value(json).expect("hit deserializes")
    }

    #[test]
    fn global_hits_render_with_their_project() {
        let hits = vec![
            hit(serde_json::json!({
                "id": "ignored", "workspace": "acme", "project": "api",
                "path": "notes/wal.md", "title": "WAL", "snippet": "wal mode", "rank": -1.5
            })),
            hit(serde_json::json!({
                "workspace": "default", "project": "web",
                "path": "notes/wal.md", "title": "WAL too", "snippet": "also wal", "rank": -1.0
            })),
        ];
        let text = render_text(&ai_memory_core::i18n::Localizer::default(), "wal", &hits);
        assert!(
            text.contains("  acme/api  notes/wal.md  rank=-1.5000\n"),
            "{text}"
        );
        assert!(
            text.contains("  default/web  notes/wal.md  rank=-1.0000\n"),
            "{text}"
        );

        let json = serde_json::to_value(&hits).unwrap();
        assert_eq!(json[0]["workspace"], "acme");
        assert_eq!(json[0]["project"], "api");
        assert_eq!(json[1]["project"], "web");
    }

    #[test]
    fn scoped_hits_render_and_serialize_as_before() {
        let hits = vec![hit(serde_json::json!({
            "id": "ignored", "path": "notes/wal.md", "title": "WAL",
            "snippet": "wal mode", "rank": -1.5
        }))];
        let text = render_text(&ai_memory_core::i18n::Localizer::default(), "wal", &hits);
        assert!(text.contains("  notes/wal.md  rank=-1.5000\n"), "{text}");
        let json = serde_json::to_value(&hits).unwrap();
        assert!(json[0].get("workspace").is_none());
        assert!(json[0].get("project").is_none());
        assert_eq!(
            render_text(&ai_memory_core::i18n::Localizer::default(), "wal", &[]),
            "no results for \"wal\"\n"
        );
    }
}
