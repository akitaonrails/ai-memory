//! Grant management for `ai-memory user grant|revoke|grants` and `ai-memory
//! project grants` (#708).
//!
//! Thin HTTP client over `/admin/users/{username}/grant|revoke|grants` and
//! `/admin/projects/grants`. The caller's bearer token must authenticate as
//! root: grants are an operator action, and the server is usually somewhere the
//! operator's laptop cannot open the database directly.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{ProjectGrantsArgs, UserGrantArgs, UserGrantsArgs, UserRevokeArgs};
use crate::commands::user::url_encode;
use crate::http_client::{ServerEndpoint, get_json, post_json};

#[derive(Debug, Serialize)]
struct GrantRequest<'a> {
    workspace: &'a str,
    project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<&'a str>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GrantRow {
    username: String,
    workspace: String,
    project: String,
    level: String,
}

#[derive(Debug, Deserialize)]
struct GrantList {
    grants: Vec<GrantRow>,
}

fn print_grants(grants: &[GrantRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(grants)?);
        return Ok(());
    }
    if grants.is_empty() {
        // Say what empty means: nothing is lost on an open project.
        println!("(no grants)");
        println!(
            "Open projects admit every user regardless; a restricted project with no \
             grants admits only root."
        );
        return Ok(());
    }
    let project_w = grants
        .iter()
        .map(|g| g.workspace.len() + 1 + g.project.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let user_w = grants
        .iter()
        .map(|g| g.username.len())
        .max()
        .unwrap_or(8)
        .max(8);
    println!("{:<project_w$}  {:<user_w$}  LEVEL", "PROJECT", "USERNAME");
    for g in grants {
        let project = format!("{}/{}", g.workspace, g.project);
        println!(
            "{project:<project_w$}  {:<user_w$}  {}",
            g.username, g.level
        );
    }
    Ok(())
}

/// `ai-memory user grants [--user NAME]`.
///
/// # Errors
/// Transport failures, a non-root token, or an unknown user.
pub async fn list_for_user(ep: &ServerEndpoint, args: &UserGrantsArgs) -> Result<()> {
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
            .context("listing grants")?,
    };
    print_grants(&resp.grants, args.json)
}

/// `ai-memory project grants --workspace W --project P`.
///
/// # Errors
/// Transport failures, a non-root token, or an unknown project.
pub async fn list_for_project(ep: &ServerEndpoint, args: &ProjectGrantsArgs) -> Result<()> {
    let resp: GrantList = get_json(
        ep,
        "/admin/projects/grants",
        &[("workspace", &args.workspace), ("project", &args.project)],
    )
    .await
    .with_context(|| format!("listing grants on {}/{}", args.workspace, args.project))?;
    print_grants(&resp.grants, args.json)
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
/// Transport failures, a non-root token, an unknown user, project or level.
pub async fn grant(ep: &ServerEndpoint, args: &UserGrantArgs) -> Result<()> {
    let body = GrantRequest {
        workspace: &args.workspace,
        project: &args.project,
        level: Some(&args.level),
    };
    let resp: GrantResponse = post_json(
        ep,
        &format!("/admin/users/{}/grant", url_encode(&args.user)),
        &body,
    )
    .await
    .with_context(|| {
        format!(
            "granting {} on {}/{}",
            args.user, args.workspace, args.project
        )
    })?;
    let who = &args.user;
    let project = format!("{}/{}", args.workspace, args.project);
    match (resp.changed, resp.previous) {
        (false, _) => println!(
            "{who} already holds {} on {project}; nothing changed.",
            resp.level
        ),
        (true, Some(previous)) => println!("{who}: {previous} -> {} on {project}.", resp.level),
        (true, None) => println!("{who} now holds {} on {project}.", resp.level),
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
/// Transport failures, a non-root token, an unknown user or project.
pub async fn revoke(ep: &ServerEndpoint, args: &UserRevokeArgs) -> Result<()> {
    let body = GrantRequest {
        workspace: &args.workspace,
        project: &args.project,
        level: None,
    };
    let resp: RevokeResponse = post_json(
        ep,
        &format!("/admin/users/{}/revoke", url_encode(&args.user)),
        &body,
    )
    .await
    .with_context(|| {
        format!(
            "revoking {} on {}/{}",
            args.user, args.workspace, args.project
        )
    })?;
    let project = format!("{}/{}", args.workspace, args.project);
    if resp.revoked {
        println!("{} no longer holds anything on {project}.", args.user);
    } else {
        println!("{} held nothing on {project}; nothing changed.", args.user);
    }
    Ok(())
}
