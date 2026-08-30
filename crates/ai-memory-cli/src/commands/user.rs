//! `ai-memory user` — manage compatibility tokens and human password login.
//!
//! Thin HTTP client over `/admin/users*`. The caller's bearer token must
//! authenticate as root. Single-token commands remain deprecated 1.x shims.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{
    UserAddArgs, UserAddHumanArgs, UserArgs, UserCommand, UserDisableArgs, UserEnableArgs,
    UserExpireArgs, UserListArgs, UserPatchArgs, UserResetPasswordArgs, UserReviveArgs,
    UserRotateTokenArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, patch_json, post_json};

#[derive(Debug, Deserialize, Serialize)]
struct UserRow {
    id: String,
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    created_at: i64,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    token_expired_at: Option<i64>,
    #[serde(default)]
    role: String,
    #[serde(default)]
    must_change_password: bool,
    #[serde(default)]
    disabled_at: Option<i64>,
    #[serde(default)]
    has_password: bool,
}

impl UserRow {
    fn status(&self) -> &'static str {
        if self.disabled_at.is_some() {
            "disabled"
        } else if self.has_password && self.must_change_password {
            "must-change"
        } else if self.has_password {
            "active"
        } else if self.token_expired_at.is_some() {
            "expired"
        } else {
            "active"
        }
    }
}

#[derive(Debug, Deserialize)]
struct UserWithToken {
    user: UserRow,
    token: String,
}

#[derive(Debug, Deserialize)]
struct UserWithPassword {
    user: UserRow,
    temporary_password: String,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    user: UserRow,
}

#[derive(Debug, Deserialize)]
struct UserList {
    users: Vec<UserRow>,
}

/// Dispatch entry point for `ai-memory user <subcommand>`.
///
/// # Errors
/// Returns an error if the HTTP call fails, the server returns non-2xx,
/// or the response body can't be deserialised.
pub async fn run(config: &Config, args: UserArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        UserCommand::Add(args) => add(&ep, args).await,
        UserCommand::AddHuman(args) => add_human(&ep, args).await,
        UserCommand::List(args) => list(&ep, args).await,
        UserCommand::Expire(args) => expire(&ep, args).await,
        UserCommand::Revive(args) => revive(&ep, args).await,
        UserCommand::RotateToken(args) => rotate_token(&ep, args).await,
        UserCommand::ResetPassword(args) => reset_password(&ep, args).await,
        UserCommand::Disable(args) => disable(&ep, args).await,
        UserCommand::Enable(args) => enable(&ep, args).await,
        UserCommand::Patch(args) => patch(&ep, args).await,
    }
}

#[derive(Debug, Serialize)]
struct CreateUserBody<'a> {
    username: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'a str>,
}

async fn add(ep: &ServerEndpoint, args: UserAddArgs) -> Result<()> {
    let body = CreateUserBody {
        username: args.username.trim(),
        name: args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        email: args
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        role: None,
    };
    let resp: UserWithToken = post_json(ep, "/admin/users", &body)
        .await
        .context("creating compatibility user")?;
    print_token(&resp, args.json, "created")
}

async fn add_human(ep: &ServerEndpoint, args: UserAddHumanArgs) -> Result<()> {
    let role = args
        .role
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let body = CreateUserBody {
        username: args.username.trim(),
        name: args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        email: args
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        role,
    };
    let resp: UserWithPassword = post_json(ep, "/admin/human-users", &body)
        .await
        .context("creating human user")?;
    print_temp_password(&resp, args.json, "created")
}

async fn list(ep: &ServerEndpoint, args: UserListArgs) -> Result<()> {
    let resp: UserList = get_json(ep, "/admin/users", &[])
        .await
        .context("listing users")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.users)?);
        return Ok(());
    }
    if resp.users.is_empty() {
        println!("(no registered users)");
        return Ok(());
    }
    let user_w = resp
        .users
        .iter()
        .map(|u| u.username.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let name_w = resp
        .users
        .iter()
        .filter_map(|u| u.name.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);
    let role_w = resp
        .users
        .iter()
        .map(|u| u.role.len())
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<user_w$}  {:<name_w$}  {:<role_w$}  {:<12}",
        "USERNAME",
        "NAME",
        "ROLE",
        "STATUS",
        user_w = user_w,
        name_w = name_w,
        role_w = role_w,
    );
    for u in &resp.users {
        println!(
            "{:<user_w$}  {:<name_w$}  {:<role_w$}  {:<12}",
            u.username,
            u.name.as_deref().unwrap_or("-"),
            u.role,
            u.status(),
            user_w = user_w,
            name_w = name_w,
            role_w = role_w,
        );
    }
    Ok(())
}

async fn expire(ep: &ServerEndpoint, args: UserExpireArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Expire token for user '{}'? Their token stops authenticating immediately. (y/N) ",
            args.username
        ))?;
    }
    let path = format!("/admin/users/{}/expire", url_encode(&args.username));
    let _: UserResponse = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("expiring user token")?;
    println!("✓ expired token for user '{}'", args.username);
    Ok(())
}

async fn revive(ep: &ServerEndpoint, args: UserReviveArgs) -> Result<()> {
    let path = format!("/admin/users/{}/revive", url_encode(&args.username));
    let _: UserResponse = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("reviving user token")?;
    println!("✓ revived token for user '{}'", args.username);
    Ok(())
}

async fn rotate_token(ep: &ServerEndpoint, args: UserRotateTokenArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Rotate token for user '{}'? Existing clients will start getting 401 immediately. (y/N) ",
            args.username
        ))?;
    }
    let path = format!("/admin/users/{}/rotate-token", url_encode(&args.username));
    let resp: UserWithToken = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("rotating user token")?;
    print_token(&resp, args.json, "rotated token for")
}

async fn reset_password(ep: &ServerEndpoint, args: UserResetPasswordArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Reset password for user '{}'? Their current password stops working immediately. (y/N) ",
            args.username
        ))?;
    }
    let path = format!("/admin/users/{}/reset-password", url_encode(&args.username));
    let resp: UserWithPassword = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("resetting password")?;
    print_temp_password(&resp, args.json, "reset password for")
}

async fn disable(ep: &ServerEndpoint, args: UserDisableArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Disable human login for user '{}'? (y/N) ",
            args.username
        ))?;
    }
    let path = format!("/admin/users/{}/disable", url_encode(&args.username));
    let _: UserResponse = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("disabling user")?;
    println!("✓ disabled user '{}'", args.username);
    Ok(())
}

async fn enable(ep: &ServerEndpoint, args: UserEnableArgs) -> Result<()> {
    let path = format!("/admin/users/{}/enable", url_encode(&args.username));
    let _: UserResponse = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("enabling user")?;
    println!("✓ enabled user '{}'", args.username);
    Ok(())
}

#[derive(Debug, Serialize)]
struct PatchUserBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
}

async fn patch(ep: &ServerEndpoint, args: UserPatchArgs) -> Result<()> {
    let body = PatchUserBody {
        name: args.name.map(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }),
        email: args.email.map(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }),
        role: args
            .role
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    let path = format!("/admin/users/{}", url_encode(&args.username));
    let resp: UserResponse = patch_json(ep, &path, &body)
        .await
        .context("patching user")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.user)?);
    } else {
        println!(
            "✓ updated user '{}' ({})",
            resp.user.username,
            resp.user.status()
        );
    }
    Ok(())
}

fn print_token(resp: &UserWithToken, json: bool, verb: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "user": &resp.user,
                "token": &resp.token
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "✓ {verb} user '{}'\n\nStore this token now — it will NOT be shown again.",
            resp.user.username
        );
        println!("{}", resp.token);
    }
    Ok(())
}

fn print_temp_password(resp: &UserWithPassword, json: bool, verb: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "user": &resp.user,
                "temporary_password": &resp.temporary_password
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "✓ {verb} user '{}'", resp.user.username);
        if let Some(name) = &resp.user.name {
            let _ = writeln!(stderr, "  name:  {name}");
        }
        if let Some(email) = &resp.user.email {
            let _ = writeln!(stderr, "  email: {email}");
        }
        let _ = writeln!(
            stderr,
            "  role:  {}\n\n\
             Store this temporary password now — it will NOT be shown again. \
             The user must change it on next login.",
            resp.user.role
        );
        println!("{}", resp.temporary_password);
    }
    Ok(())
}

fn confirm(prompt: &str) -> Result<()> {
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    drop(stderr);

    let mut buf = String::new();
    let n = io::stdin().lock().read_line(&mut buf)?;
    if n == 0 {
        bail!("aborted (no input)");
    }
    let trimmed = buf.trim();
    if !trimmed.eq_ignore_ascii_case("y") && !trimmed.eq_ignore_ascii_case("yes") {
        bail!("aborted");
    }
    Ok(())
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_passes_safe_chars_through() {
        assert_eq!(url_encode("alice"), "alice");
        assert_eq!(url_encode("user_1"), "user_1");
        assert_eq!(url_encode("a.b-c"), "a.b-c");
    }

    #[test]
    fn url_encode_percent_encodes_at_and_other_specials() {
        assert_eq!(url_encode("alice@home"), "alice%40home");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("a b"), "a%20b");
    }
}
