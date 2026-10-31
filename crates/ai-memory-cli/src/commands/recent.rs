//! `ai-memory recent` — list the most recently updated wiki pages.
//!
//! Thin HTTP client. Calls `GET /admin/recent?limit=…`; renders the hits as
//! human text or JSON. Never opens the store directly.
//!
//! The scope comes from the cwd (or `--workspace`/`--project`) unless
//! `--global` is passed: then no scope is sent, the server lists every
//! project's latest pages, and each hit carries its `workspace`/`project`.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::RecentArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Recency hit, mirrors the `/admin/recent` fields used here.
#[derive(Debug, Deserialize, Serialize)]
struct Hit {
    /// Set only on a global listing: the workspace the page lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    /// Set only on a global listing: the project the page lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    path: String,
    title: String,
    snippet: String,
    updated_at: String,
}

/// Run the `recent` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or returns non-2xx.
pub async fn run(config: &Config, args: RecentArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let limit_str = args.limit.to_string();
    let mut query = vec![("limit", limit_str)];
    if !args.global {
        let (workspace, project) =
            super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
        query.push(("workspace", workspace));
        query.push(("project", project));
    }
    let query: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let hits: Vec<Hit> = get_json(&ep, "/admin/recent", &query).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        print!(
            "{}",
            render_text(&config.i18n, &hits, jiff::Timestamp::now())
        );
    }
    Ok(())
}

fn render_text(
    i18n: &ai_memory_core::i18n::Localizer,
    hits: &[Hit],
    now: jiff::Timestamp,
) -> String {
    use std::fmt::Write as _;
    if hits.is_empty() {
        return i18n.text("cli-recent-no-recent-pages");
    }
    let mut out = i18n.format(
        "cli-recent-page-s",
        &[("value0", format!("{}", hits.len()))],
    );
    for hit in hits {
        let age = age_label(i18n, &hit.updated_at, now)
            .map(|age| format!("  ({age})"))
            .unwrap_or_default();
        let _ = match (&hit.workspace, &hit.project) {
            (Some(workspace), Some(project)) => {
                writeln!(
                    out,
                    "  {workspace}/{project}  {}  {}{age}",
                    hit.path, hit.updated_at
                )
            }
            _ => writeln!(out, "  {}  {}{age}", hit.path, hit.updated_at),
        };
        let _ = writeln!(out, "    {}", hit.title);
        let _ = writeln!(out, "    {}", hit.snippet);
    }
    out
}

fn age_label(
    i18n: &ai_memory_core::i18n::Localizer,
    updated_at: &str,
    now: jiff::Timestamp,
) -> Option<String> {
    let then: jiff::Timestamp = updated_at.parse().ok()?;
    Some(i18n.age((now - then).get_seconds()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(json: serde_json::Value) -> Hit {
        serde_json::from_value(json).expect("hit deserializes")
    }

    fn now() -> jiff::Timestamp {
        "2026-10-17T15:12:00Z".parse().expect("fixed now")
    }

    #[test]
    fn global_hits_render_with_their_project_and_age() {
        let hits = vec![
            hit(serde_json::json!({
                "id": "ignored", "workspace": "acme", "project": "api",
                "path": "notes/wal.md", "title": "WAL", "snippet": "wal mode",
                "updated_at": "2026-10-17T15:10:00Z"
            })),
            hit(serde_json::json!({
                "workspace": "default", "project": "web",
                "path": "notes/wal.md", "title": "WAL too", "snippet": "also wal",
                "updated_at": "2026-10-17T12:12:00Z"
            })),
        ];
        let text = render_text(&ai_memory_core::i18n::Localizer::default(), &hits, now());
        assert!(
            text.contains("  acme/api  notes/wal.md  2026-10-17T15:10:00Z  (2 minutes ago)\n"),
            "{text}"
        );
        assert!(
            text.contains("  default/web  notes/wal.md  2026-10-17T12:12:00Z  (3 hours ago)\n"),
            "{text}"
        );

        let json = serde_json::to_value(&hits).unwrap();
        assert_eq!(json[0]["workspace"], "acme");
        assert_eq!(json[0]["project"], "api");
        assert_eq!(json[1]["project"], "web");
        assert!(json[0].get("rank").is_none());
    }

    #[test]
    fn scoped_hits_render_and_serialize_without_scope() {
        let hits = vec![hit(serde_json::json!({
            "id": "ignored", "path": "notes/wal.md", "title": "WAL",
            "snippet": "wal mode", "updated_at": "2026-10-17T15:12:00Z"
        }))];
        let text = render_text(&ai_memory_core::i18n::Localizer::default(), &hits, now());
        assert!(
            text.contains("  notes/wal.md  2026-10-17T15:12:00Z  (just now)\n"),
            "{text}"
        );
        let json = serde_json::to_value(&hits).unwrap();
        assert!(json[0].get("workspace").is_none());
        assert!(json[0].get("project").is_none());
        assert_eq!(json[0]["updated_at"], "2026-10-17T15:12:00Z");
        assert_eq!(
            render_text(&ai_memory_core::i18n::Localizer::default(), &[], now()),
            "no recent pages\n"
        );
    }
}
