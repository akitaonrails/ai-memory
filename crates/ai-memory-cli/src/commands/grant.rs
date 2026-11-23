//! Grant management for `ai-memory user grant|revoke|grants` and `ai-memory
//! project grants` (#708). A grant covers one project, or with
//! `--all-projects` every project of a workspace.
//!
//! Thin HTTP client over `/admin/users/{username}/grant|revoke|grants` and
//! `/admin/projects/grants`. The caller's bearer token must authenticate as
//! root: grants are an operator action, and the server is usually somewhere the
//! operator's laptop cannot open the database directly.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::show::terminal_text;
use crate::cli::{ProjectGrantsArgs, UserGrantArgs, UserGrantsArgs, UserRevokeArgs};
use crate::commands::user::url_encode;
use crate::http_client::{ServerEndpoint, get_json, post_json};

#[derive(Debug, Serialize)]
struct GrantRequest<'a> {
    workspace: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    /// Sent only for a workspace grant, so a server that predates it refuses
    /// the request (no `project`) instead of granting something else.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    all_projects: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<&'a str>,
}

/// What `user grant` / `user revoke` point at, after the flags are checked.
struct Target<'a> {
    project: Option<&'a str>,
    all_projects: bool,
    /// How the target reads in a message: `workspace/project`, or the
    /// localized "every project of workspace".
    label: String,
}

/// Check `--project` / `--all-projects` and name the target.
///
/// clap already requires one of the two; this also refuses an empty
/// `--project`, which clap accepts.
fn target<'a>(
    i18n: &ai_memory_core::i18n::Localizer,
    workspace: &str,
    project: Option<&'a str>,
    all_projects: bool,
) -> Result<Target<'a>> {
    super::refuse_empty_explicit(project, "--project")?;
    let label = match project {
        Some(project) => format!("{workspace}/{project}"),
        None => crate::localization::format(
            i18n,
            "cli-grant-every-project-of-workspace",
            &[("value0", terminal_text(workspace))],
        ),
    };
    Ok(Target {
        project,
        all_projects,
        label,
    })
}

#[derive(Debug, Deserialize, Serialize)]
struct GrantRow {
    username: String,
    workspace: String,
    project: String,
    level: String,
}

/// A grant on every project of a workspace, as the listing routes return it.
#[derive(Debug, Deserialize, Serialize)]
struct WorkspaceGrantRow {
    username: String,
    workspace: String,
    level: String,
}

#[derive(Debug, Deserialize)]
struct GrantList {
    grants: Vec<GrantRow>,
    /// Absent from a server that predates workspace grants.
    #[serde(default)]
    workspace_grants: Vec<WorkspaceGrantRow>,
}

/// Table rows for a listing: workspace grants first, shown as `workspace/*`,
/// then the project grants.
fn table_rows(list: &GrantList) -> Vec<GrantRow> {
    list.workspace_grants
        .iter()
        .map(|g| GrantRow {
            username: g.username.clone(),
            workspace: g.workspace.clone(),
            project: "*".to_owned(),
            level: g.level.clone(),
        })
        .chain(list.grants.iter().map(|g| GrantRow {
            username: g.username.clone(),
            workspace: g.workspace.clone(),
            project: g.project.clone(),
            level: g.level.clone(),
        }))
        .collect()
}

fn print_grants(
    i18n: &ai_memory_core::i18n::Localizer,
    list: &GrantList,
    json: bool,
) -> Result<()> {
    if json {
        // The project grants keep the array shape scripts already parse. With
        // workspace grants in force the output is an object naming both, so a
        // script cannot read a partial answer as the whole one.
        if list.workspace_grants.is_empty() {
            println!("{}", serde_json::to_string_pretty(&list.grants)?);
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "grants": list.grants,
                    "workspace_grants": list.workspace_grants,
                }))?
            );
        }
        return Ok(());
    }
    let grants = &table_rows(list)[..];
    if grants.is_empty() {
        // Say what empty means: nothing is lost on an open project.
        println!("{}", i18n.text("cli-grant-no-grants"));
        println!(
            "{}", i18n.text("cli-grant-open-projects-admit-every-user-regardless-a-restricted-project-with-no-grant"));
        return Ok(());
    }
    // Written through a locked stdout handle rather than `print!` so the table
    // leaves the process in one write, as `project_init` already does for its
    // own bulk output.
    write_grants(i18n, grants, &mut std::io::stdout().lock())
}

/// Lay out the grants table using terminal display-cell widths. Workspace and
/// project names come from a checkout's directory basename or a marker's
/// `project` key, so they are free-form UTF-8.
fn write_grants(
    i18n: &ai_memory_core::i18n::Localizer,
    grants: &[GrantRow],
    out: &mut impl std::io::Write,
) -> Result<()> {
    let project_w = grants
        .iter()
        .map(|g| {
            unicode_width::UnicodeWidthStr::width(
                terminal_text(&format!("{}/{}", g.workspace, g.project)).as_str(),
            )
        })
        .max()
        .unwrap_or(7)
        .max(7);
    let user_w = grants
        .iter()
        .map(|g| unicode_width::UnicodeWidthStr::width(terminal_text(&g.username).as_str()))
        .max()
        .unwrap_or(8)
        .max(8);
    writeln!(
        out,
        "{}",
        crate::localization::diagnostic(
            i18n,
            format!(
                "{}  {}  LEVEL",
                pad_cell("PROJECT", project_w),
                pad_cell("USERNAME", user_w)
            )
        )
    )?;
    for g in grants {
        let project = format!("{}/{}", g.workspace, g.project);
        writeln!(
            out,
            "{}  {}  {}",
            pad_cell(&terminal_text(&project), project_w),
            pad_cell(&terminal_text(&g.username), user_w),
            terminal_text(&g.level)
        )?;
    }
    Ok(())
}

fn pad_cell(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(unicode_width::UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}

/// `ai-memory user grants [--user NAME]`.
///
/// # Errors
/// Transport failures, a non-root token, or an unknown user.
pub async fn list_for_user(ep: &ServerEndpoint, args: &UserGrantsArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let resp: GrantList = match &args.user {
        Some(user) => get_json(
            ep,
            &format!("/admin/users/{}/grants", url_encode(user)),
            &[],
        )
        .await
        .with_context(|| format!("listing {user}'s grants"))?,
        None => get_json(ep, "/admin/projects/grants", &[])
            .await
            .context(i18n.text("cli-grant-listing-grants"))?,
    };
    print_grants(i18n, &resp, args.json)
}

/// `ai-memory project grants --workspace W --project P`.
///
/// # Errors
/// Empty `--workspace` or `--project`; transport failures, a non-root token,
/// or an unknown project.
pub async fn list_for_project(ep: &ServerEndpoint, args: &ProjectGrantsArgs) -> Result<()> {
    let i18n = &ep.i18n;
    super::refuse_empty_explicit(Some(args.workspace.as_str()), "--workspace")?;
    super::refuse_empty_explicit(Some(args.project.as_str()), "--project")?;
    let resp: GrantList = get_json(
        ep,
        "/admin/projects/grants",
        &[("workspace", &args.workspace), ("project", &args.project)],
    )
    .await
    .with_context(|| format!("listing grants on {}/{}", args.workspace, args.project))?;
    print_grants(i18n, &resp, args.json)
}

#[derive(Debug, Deserialize)]
struct GrantResponse {
    level: String,
    changed: bool,
    previous: Option<String>,
}

/// `ai-memory user grant --user U --workspace W --project P --level L`.
///
/// # Errors
/// Empty `--user`, `--workspace`, `--project`, or `--level`; transport
/// failures, a non-root token, an unknown user, project or level.
pub async fn grant(ep: &ServerEndpoint, args: &UserGrantArgs) -> Result<()> {
    let i18n = &ep.i18n;
    super::refuse_empty_explicit(Some(args.user.as_str()), "--user")?;
    super::refuse_empty_explicit(Some(args.workspace.as_str()), "--workspace")?;
    let target = target(
        i18n,
        &args.workspace,
        args.project.as_deref(),
        args.all_projects,
    )?;
    super::refuse_empty_explicit(Some(args.level.as_str()), "--level")?;
    let body = GrantRequest {
        workspace: &args.workspace,
        project: target.project,
        all_projects: target.all_projects,
        level: Some(&args.level),
    };
    let resp: GrantResponse = post_json(
        ep,
        &format!("/admin/users/{}/grant", url_encode(&args.user)),
        &body,
    )
    .await
    .with_context(|| format!("granting {} on {}", args.user, target.label))?;
    let who = &args.user;
    let project = target.label;
    match (resp.changed, resp.previous) {
        (false, _) => println!(
            "{}",
            crate::localization::format(
                i18n,
                "cli-grant-who-already-holds-on-project-nothing-changed",
                &[
                    ("value0", who.to_string()),
                    ("value1", terminal_text(&resp.level)),
                    ("value2", project.to_string())
                ]
            )
        ),
        (true, Some(previous)) => println!(
            "{}",
            crate::localization::format(
                i18n,
                "cli-grant-who-previous-on-project",
                &[
                    ("value0", who.to_string()),
                    ("value1", terminal_text(&previous)),
                    ("value2", terminal_text(&resp.level)),
                    ("value3", project.to_string())
                ]
            )
        ),
        (true, None) => println!(
            "{}",
            crate::localization::format(
                i18n,
                "cli-grant-who-now-holds-on-project",
                &[
                    ("value0", who.to_string()),
                    ("value1", terminal_text(&resp.level)),
                    ("value2", project.to_string())
                ]
            )
        ),
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RevokeResponse {
    revoked: bool,
}

/// `ai-memory user revoke --user U --workspace W --project P`.
///
/// # Errors
/// Empty `--user`, `--workspace`, or `--project`; transport failures, a
/// non-root token, an unknown user or project.
pub async fn revoke(ep: &ServerEndpoint, args: &UserRevokeArgs) -> Result<()> {
    let i18n = &ep.i18n;
    super::refuse_empty_explicit(Some(args.user.as_str()), "--user")?;
    super::refuse_empty_explicit(Some(args.workspace.as_str()), "--workspace")?;
    let target = target(
        i18n,
        &args.workspace,
        args.project.as_deref(),
        args.all_projects,
    )?;
    let body = GrantRequest {
        workspace: &args.workspace,
        project: target.project,
        all_projects: target.all_projects,
        level: None,
    };
    let resp: RevokeResponse = post_json(
        ep,
        &format!("/admin/users/{}/revoke", url_encode(&args.user)),
        &body,
    )
    .await
    .with_context(|| format!("revoking {} on {}", args.user, target.label))?;
    let project = target.label;
    if resp.revoked {
        println!(
            "{}",
            crate::localization::format(
                i18n,
                "cli-grant-no-longer-holds-anything-on-project",
                &[
                    ("value0", args.user.to_string()),
                    ("value1", project.to_string())
                ]
            )
        );
    } else {
        println!(
            "{}",
            crate::localization::format(
                i18n,
                "cli-grant-held-nothing-on-project-nothing-changed",
                &[
                    ("value0", args.user.to_string()),
                    ("value1", project.to_string())
                ]
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(project: &str, username: &str) -> GrantRow {
        GrantRow {
            username: username.to_owned(),
            workspace: "default".to_owned(),
            project: project.to_owned(),
            level: "read".to_owned(),
        }
    }

    /// The display-cell offset at which `line` carries `username`, or `None`.
    fn username_column(line: &str, username: &str) -> Option<usize> {
        line.find(username)
            .map(|byte| unicode_width::UnicodeWidthStr::width(&line[..byte]))
    }

    #[test]
    fn grants_table_columns_line_up_past_ascii() {
        let rows = [grant("caf\u{e9}", "ana"), grant("web", "bob")];
        let mut buf = Vec::new();
        write_grants(&ai_memory_core::i18n::Localizer::default(), &rows, &mut buf)
            .expect("rendering to a buffer cannot fail");
        let table = String::from_utf8(buf).expect("the table is utf-8");
        let header = table.lines().next().expect("a header row");
        // The header is ASCII, so its byte offset is its character offset.
        let user_col = header.find("USERNAME").expect("a USERNAME column");
        // `café` is one byte longer than it is wide, so a byte-measured width
        // pads that row one column too far and shifts USERNAME and LEVEL.
        for (line, row) in table.lines().skip(1).zip(rows.iter()) {
            assert_eq!(
                username_column(line, &row.username),
                Some(user_col),
                "USERNAME column moved on {line:?}:\n{table}"
            );
        }
        // Control: an ASCII-only table lines up under either measure, so the
        // non-ASCII row above is what carries the assertion.
        let ascii_rows = [grant("cafe", "ana"), grant("web", "bob")];
        let mut ascii_buf = Vec::new();
        write_grants(
            &ai_memory_core::i18n::Localizer::default(),
            &ascii_rows,
            &mut ascii_buf,
        )
        .expect("rendering to a buffer cannot fail");
        let ascii = String::from_utf8(ascii_buf).expect("the table is utf-8");
        for (line, row) in ascii.lines().skip(1).zip(ascii_rows.iter()) {
            assert_eq!(
                username_column(line, &row.username),
                Some(user_col),
                "ascii rows must line up as well:\n{ascii}"
            );
        }

        let cjk_rows = [grant("記憶管理", "利用者"), grant("web", "bob")];
        let mut cjk_buf = Vec::new();
        write_grants(
            &ai_memory_core::i18n::Localizer::default(),
            &cjk_rows,
            &mut cjk_buf,
        )
        .unwrap();
        let cjk = String::from_utf8(cjk_buf).unwrap();
        let cjk_user_col = username_column(cjk.lines().next().unwrap(), "USERNAME").unwrap();
        for (line, row) in cjk.lines().skip(1).zip(cjk_rows.iter()) {
            assert_eq!(
                username_column(line, &row.username),
                Some(cjk_user_col),
                "{cjk}"
            );
        }
    }
    fn grants_args() -> ProjectGrantsArgs {
        ProjectGrantsArgs {
            workspace: "default".into(),
            project: "app".into(),
            json: false,
        }
    }

    fn dead_ep() -> ServerEndpoint {
        ServerEndpoint::from_pair(Some("http://127.0.0.1:1".into()), None)
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_listing_grants() {
        let mut a = grants_args();
        a.project = "   ".into();
        let error = list_for_project(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_listing_grants() {
        let mut a = grants_args();
        a.workspace = "".into();
        let error = list_for_project(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }
    /// The request for one project is byte-for-byte what it was before
    /// workspace grants existed, and a workspace grant says so by name and
    /// carries no `project`.
    #[test]
    fn the_request_names_one_project_or_all_projects_never_both() {
        let one = GrantRequest {
            workspace: "acme",
            project: Some("api"),
            all_projects: false,
            level: Some("read"),
        };
        assert_eq!(
            serde_json::to_value(&one).unwrap(),
            serde_json::json!({"workspace": "acme", "project": "api", "level": "read"})
        );
        let all = GrantRequest {
            workspace: "acme",
            project: None,
            all_projects: true,
            level: Some("read"),
        };
        assert_eq!(
            serde_json::to_value(&all).unwrap(),
            serde_json::json!({"workspace": "acme", "all_projects": true, "level": "read"})
        );
    }

    #[test]
    fn workspace_grants_lead_the_table_as_workspace_star() {
        let list: GrantList = serde_json::from_value(serde_json::json!({
            "grants": [
                {"username": "bob", "workspace": "acme", "project": "api", "level": "read"},
            ],
            "workspace_grants": [
                {"username": "ana", "workspace": "acme", "level": "write"},
            ],
        }))
        .unwrap();
        let mut buf = Vec::new();
        write_grants(
            &ai_memory_core::i18n::Localizer::default(),
            &table_rows(&list),
            &mut buf,
        )
        .unwrap();
        let table = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[1].starts_with("acme/* "), "{table}");
        assert!(lines[1].ends_with("write"), "{table}");
        assert!(lines[2].starts_with("acme/api"), "{table}");

        // A server that predates workspace grants sends no such key.
        let old: GrantList = serde_json::from_value(serde_json::json!({"grants": []})).unwrap();
        assert!(table_rows(&old).is_empty());
    }

    #[tokio::test]
    async fn an_all_projects_grant_still_refuses_an_empty_workspace_and_level() {
        let all = || UserGrantArgs {
            project: None,
            all_projects: true,
            ..grant_args()
        };
        let mut a = all();
        a.workspace = " ".into();
        let error = super::grant(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
        let mut a = all();
        a.level = "".into();
        let error = super::grant(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--level was passed empty"),
            "unexpected error: {error}"
        );
    }

    fn grant_args() -> UserGrantArgs {
        UserGrantArgs {
            user: "ana".into(),
            workspace: "default".into(),
            project: Some("app".into()),
            all_projects: false,
            level: "write".into(),
        }
    }

    fn revoke_args() -> UserRevokeArgs {
        UserRevokeArgs {
            user: "ana".into(),
            workspace: "default".into(),
            project: Some("app".into()),
            all_projects: false,
        }
    }

    #[tokio::test]
    async fn an_empty_user_flag_is_refused_before_granting() {
        let mut a = grant_args();
        a.user = "   ".into();
        let error = super::grant(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--user was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_granting() {
        let mut a = grant_args();
        a.workspace = "".into();
        let error = super::grant(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_granting() {
        let mut a = grant_args();
        a.project = Some("  ".into());
        let error = super::grant(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_level_flag_is_refused_before_granting() {
        let mut a = grant_args();
        a.level = "".into();
        let error = super::grant(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--level was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_user_flag_is_refused_before_revoking() {
        let mut a = revoke_args();
        a.user = "   ".into();
        let error = super::revoke(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--user was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_revoking() {
        let mut a = revoke_args();
        a.workspace = "".into();
        let error = super::revoke(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_revoking() {
        let mut a = revoke_args();
        a.project = Some("  ".into());
        let error = super::revoke(&dead_ep(), &a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }
}
