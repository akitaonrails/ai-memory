//! `ai-memory pending-writes` — review staged auto-improvement proposals.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{
    PendingWriteIdArgs, PendingWriteRejectArgs, PendingWritesArgs, PendingWritesCommand,
    PendingWritesListArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json_with_query};

#[derive(Debug, Deserialize, Serialize)]
struct ProposalSummary {
    id: String,
    status: String,
    operation: String,
    target_path: String,
    kind: String,
    title: String,
    confidence: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProposalDetail {
    summary: ProposalSummary,
    rationale: String,
    body_markdown: String,
    events: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DiffResponse {
    diff: String,
}

pub async fn run(config: &Config, args: PendingWritesArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        PendingWritesCommand::List(args) => list(config, &ep, args).await,
        PendingWritesCommand::Show(args) => show(config, &ep, args).await,
        PendingWritesCommand::Diff(args) => diff(config, &ep, args).await,
        PendingWritesCommand::Approve(args) => approve(config, &ep, args).await,
        PendingWritesCommand::Reject(args) => reject(config, &ep, args).await,
    }
}

async fn list(config: &Config, ep: &ServerEndpoint, args: PendingWritesListArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let limit = args.limit.to_string();
    let mut query = vec![
        ("workspace", workspace.as_str()),
        ("project", project.as_str()),
        ("limit", limit.as_str()),
    ];
    if let Some(status) = args.status.as_deref() {
        query.push(("status", status));
    }
    let proposals: Vec<ProposalSummary> = get_json(ep, "/admin/pending-writes", &query).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&proposals)?);
    } else if proposals.is_empty() {
        println!("(no pending writes)");
    } else {
        for p in proposals {
            println!("{}  {}  {}  {}", p.id, p.status, p.operation, p.target_path);
        }
    }
    Ok(())
}

async fn show(config: &Config, ep: &ServerEndpoint, args: PendingWriteIdArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let detail: ProposalDetail = get_json(
        ep,
        &format!("/admin/pending-writes/{}", args.id),
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        println!(
            "{} [{}] {}",
            detail.summary.target_path, detail.summary.status, detail.summary.title
        );
        println!(
            "\n{}\n\n--- body ---\n{}",
            detail.rationale, detail.body_markdown
        );
    }
    Ok(())
}

async fn diff(config: &Config, ep: &ServerEndpoint, args: PendingWriteIdArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let resp: DiffResponse = get_json(
        ep,
        &format!("/admin/pending-writes/{}/diff", args.id),
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print!("{}", resp.diff);
    }
    Ok(())
}

async fn approve(config: &Config, ep: &ServerEndpoint, args: PendingWriteIdArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let resp: serde_json::Value = post_json_with_query(
        ep,
        &format!("/admin/pending-writes/{}/approve", args.id),
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
        &serde_json::json!({}),
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("✓ approved {}", args.id);
    }
    Ok(())
}

async fn reject(config: &Config, ep: &ServerEndpoint, args: PendingWriteRejectArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let resp: serde_json::Value = post_json_with_query(
        ep,
        &format!("/admin/pending-writes/{}/reject", args.id),
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
        &serde_json::json!({ "reason": args.reason }),
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("✓ rejected {}", args.id);
    }
    Ok(())
}
