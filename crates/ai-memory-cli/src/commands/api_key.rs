//! `ai-memory api-key` — manage native `aim_` API credentials.
//!
//! Thin HTTP client over `/admin/api-credentials*`. Root bearer required.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{
    ApiKeyAddArgs, ApiKeyArgs, ApiKeyCommand, ApiKeyListArgs, ApiKeyRevokeArgs, ApiKeyRotateArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

#[derive(Debug, Deserialize, Serialize)]
struct CredentialRow {
    id: String,
    user_id: String,
    label: String,
    #[serde(default)]
    preview: Option<String>,
    created_at: i64,
    #[serde(default)]
    last_used_at: Option<i64>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    revoked_at: Option<i64>,
}

impl CredentialRow {
    fn status(&self) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else {
            "active"
        }
    }
}

#[derive(Debug, Deserialize)]
struct CredentialWithToken {
    credential: CredentialRow,
    token: String,
}

#[derive(Debug, Deserialize)]
struct CredentialList {
    credentials: Vec<CredentialRow>,
}

/// Dispatch `ai-memory api-key`.
///
/// # Errors
/// HTTP failure or malformed JSON.
pub async fn run(config: &Config, args: ApiKeyArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        ApiKeyCommand::Add(args) => add(&ep, args).await,
        ApiKeyCommand::List(args) => list(&ep, args).await,
        ApiKeyCommand::Rotate(args) => rotate(&ep, args).await,
        ApiKeyCommand::Revoke(args) => revoke(&ep, args).await,
    }
}

#[derive(Debug, Serialize)]
struct CreateBody<'a> {
    username: &'a str,
    label: &'a str,
}

async fn add(ep: &ServerEndpoint, args: ApiKeyAddArgs) -> Result<()> {
    let body = CreateBody {
        username: args.username.trim(),
        label: args.label.trim(),
    };
    let resp: CredentialWithToken = post_json(ep, "/admin/api-credentials", &body)
        .await
        .context("creating API credential")?;
    print_token(&resp, args.json, "created")
}

async fn list(ep: &ServerEndpoint, args: ApiKeyListArgs) -> Result<()> {
    let resp: CredentialList = get_json(ep, "/admin/api-credentials", &[])
        .await
        .context("listing API credentials")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.credentials)?);
        return Ok(());
    }
    if resp.credentials.is_empty() {
        println!("(no API credentials)");
        return Ok(());
    }
    let id_w = resp
        .credentials
        .iter()
        .map(|c| c.id.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let label_w = resp
        .credentials
        .iter()
        .map(|c| c.label.len())
        .max()
        .unwrap_or(5)
        .max(5);
    println!(
        "{:<id_w$}  {:<label_w$}  {:<8}  PREVIEW",
        "ID",
        "LABEL",
        "STATUS",
        id_w = id_w,
        label_w = label_w,
    );
    for c in &resp.credentials {
        println!(
            "{:<id_w$}  {:<label_w$}  {:<8}  {}",
            c.id,
            c.label,
            c.status(),
            c.preview.as_deref().unwrap_or("-"),
            id_w = id_w,
            label_w = label_w,
        );
    }
    Ok(())
}

async fn rotate(ep: &ServerEndpoint, args: ApiKeyRotateArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Rotate API key '{}'? Existing clients using the old secret will 401 immediately. (y/N) ",
            args.id
        ))?;
    }
    let path = format!("/admin/api-credentials/{}/rotate", args.id.trim());
    let resp: CredentialWithToken = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("rotating API credential")?;
    print_token(&resp, args.json, "rotated")
}

async fn revoke(ep: &ServerEndpoint, args: ApiKeyRevokeArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!("Revoke API key '{}'? (y/N) ", args.id))?;
    }
    let path = format!("/admin/api-credentials/{}/revoke", args.id.trim());
    let _: serde_json::Value = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("revoking API credential")?;
    println!("✓ revoked API key '{}'", args.id);
    Ok(())
}

fn print_token(resp: &CredentialWithToken, json: bool, verb: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "credential": &resp.credential,
                "token": &resp.token
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "✓ {verb} API key '{}' ({})\n\n\
             Store this token now — it will NOT be shown again.",
            resp.credential.id, resp.credential.label
        );
        println!("{}", resp.token);
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
