//! `ai-memory merge-project` — losslessly join two project identities.

use std::str::FromStr as _;

use ai_memory_core::ProjectId;
use anyhow::{Result, bail};
use serde::Serialize;

use super::show::terminal_text;
use crate::cli::MergeProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct MergeProjectRequest {
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    into: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    into_project_id: Option<String>,
    confirm: bool,
}

/// Run the `merge-project` subcommand.
pub async fn run(config: &Config, args: MergeProjectArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.from.as_deref(), "--from")?;
    super::refuse_empty_explicit(args.from_project_id.as_deref(), "--from-project-id")?;
    super::refuse_empty_explicit(args.into.as_deref(), "--into")?;
    super::refuse_empty_explicit(args.into_project_id.as_deref(), "--into-project-id")?;
    if args.from.is_none() == args.from_project_id.is_none() {
        bail!(
            "{}",
            i18n.text("cli-merge-project-exactly-one-of-from-or-from-project-id-is-required")
        );
    }
    if args.into.is_none() == args.into_project_id.is_none() {
        bail!(
            "{}",
            i18n.text("cli-merge-project-exactly-one-of-into-or-into-project-id-is-required")
        );
    }
    // The workspace can come from this checkout's marker, so the merge goes
    // to the server that marker selects, like rename-project and
    // move-project (#2016). A profile that does not resolve fails here,
    // before anything is sent.
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let workspace = super::resolve_workspace(config, args.workspace.as_deref())?;
    let request = MergeProjectRequest {
        workspace,
        from: args.from,
        from_project_id: args.from_project_id,
        into: args.into,
        into_project_id: args.into_project_id,
        confirm: args.confirm,
    };
    let report: serde_json::Value = post_json(&endpoint, "/admin/merge-project", &request).await?;

    let verb = if report["dry_run"].as_bool().unwrap_or(true) {
        "Would merge"
    } else {
        "Merged"
    };
    println!(
        "{}", i18n.format("cli-merge-project-verb-page-versions-sessions-observations-handoffs-workstreams-page-conflicts", &[("value0", verb.to_string()), ("value1", terminal_text(report["from"].as_str().unwrap_or("source"))), ("value2", terminal_text(report["into"].as_str().unwrap_or("target"))), ("value3", format!("{}", report["page_versions"].as_u64().unwrap_or(0))), ("value4", format!("{}", report["sessions"].as_u64().unwrap_or(0))), ("value5", format!("{}", report["observations"].as_u64().unwrap_or(0))), ("value6", format!("{}", report["handoffs"].as_u64().unwrap_or(0))), ("value7", format!("{}", report["workstreams"].as_u64().unwrap_or(0))), ("value8", format!("{}", report["page_conflicts"].as_u64().unwrap_or(0)))]));
    if !args.confirm {
        println!(
            "{}",
            i18n.text("cli-merge-project-re-run-the-same-command-with-confirm-to-apply-this-merge")
        );
    }
    if let Some(warning) = report["manifest_warning"].as_str() {
        eprintln!("{}", i18n.format("cli-merge-project-warning-merge-committed-but-scope-manifest-refresh-failed-warning", &[("value0", terminal_text(warning))]));
    }
    if args.confirm
        && let Err(error) = repoint_local_checkout(config, &endpoint, &request, &report)
    {
        eprintln!(
            "{}",
            crate::localization::format(
                i18n,
                "cli-merge-project-ai-memory-projects-merged-but-the-client-local-checkout-link-could-not-be-up",
                &[("value0", format!("{:#}", error))]
            )
        );
    }
    Ok(())
}

/// Point this client's checkout link and saved project bindings at the merge
/// target, as `repair-split` does for the split it joins. The source project
/// is gone, so a binding left on its id gets 404 on every launch from that
/// checkout.
fn repoint_local_checkout(
    config: &Config,
    endpoint: &ServerEndpoint,
    request: &MergeProjectRequest,
    report: &serde_json::Value,
) -> Result<()> {
    let workspace = request.workspace.as_str();
    let Some(from) = reported_project(&report["from"], workspace) else {
        return Ok(());
    };
    let Some(into) = reported_project(&report["into"], workspace) else {
        return Ok(());
    };
    super::project_registry::rekey_scope(config, endpoint, workspace, from, workspace, into)?;
    let from_id = merged_id(&report["from_project_id"], &request.from_project_id);
    let into_id = merged_id(&report["into_project_id"], &request.into_project_id);
    if let (Some(from_id), Some(into_id)) = (from_id, into_id) {
        super::project_registry::rebind_project(config, endpoint, from_id, into_id, into)?;
    }
    Ok(())
}

/// The project name in a report label. The server writes the label as
/// `<workspace>/<project>`, with the workspace this client sent.
fn reported_project<'a>(label: &'a serde_json::Value, workspace: &str) -> Option<&'a str> {
    let rest = label.as_str()?.strip_prefix(workspace)?;
    rest.strip_prefix('/')
}

/// One side's project id as the server resolved it, or the id the operator
/// passed when the server predates the field.
fn merged_id(reported: &serde_json::Value, requested: &Option<String>) -> Option<ProjectId> {
    ProjectId::from_str(reported.as_str().or(requested.as_deref())?).ok()
}

#[cfg(test)]
mod tests {
    use ai_memory_workstream::inspect_repository_fingerprints;

    use super::*;
    use crate::commands::project_registry::{
        links_for_server, project_binding, record_prepared_checkout, record_project_binding,
    };

    /// Bind a checkout to `source` as `default/acme-api`, apply the local
    /// re-pointing for a merge into `api`, and return the project the checkout
    /// selects afterwards with the name its link is filed under.
    fn after_merge(
        source: ProjectId,
        request: &MergeProjectRequest,
        report: &serde_json::Value,
    ) -> (ProjectId, String) {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&checkout)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success()
        );
        let config = Config {
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        let endpoint = ServerEndpoint::from_pair(Some("http://memory:49374".to_owned()), None);
        let scope = ai_memory_core::ManagedProjectScope {
            workspace_id: ai_memory_core::WorkspaceId::new(),
            project_id: source,
            workspace: "default".into(),
            project: "acme-api".into(),
        };
        let fingerprints = inspect_repository_fingerprints(&checkout).unwrap();
        record_project_binding(
            &config,
            &endpoint,
            "acme-api",
            &checkout,
            &scope,
            &fingerprints,
        )
        .unwrap();
        record_prepared_checkout(&config, &endpoint, "default", "acme-api", &checkout).unwrap();

        repoint_local_checkout(&config, &endpoint, request, report).unwrap();

        let bound = project_binding(&config, &endpoint, "default", "acme-api", &checkout)
            .unwrap()
            .unwrap();
        let links = links_for_server(&config, &endpoint).unwrap();
        assert_eq!(links.len(), 1);
        (bound.project_id, links[0].project.clone())
    }

    #[test]
    fn reported_project_reads_the_name_after_the_workspace() {
        let label = serde_json::json!("default/acme-api");
        assert_eq!(reported_project(&label, "default"), Some("acme-api"));
        // A workspace that only shares a prefix, and a label that is not text.
        assert_eq!(reported_project(&label, "def"), None);
        let missing = serde_json::Value::Null;
        assert_eq!(reported_project(&missing, "default"), None);
    }

    /// A checkout bound to the merged-away project selects the target
    /// afterwards, and its link is filed under the target's name.
    #[test]
    fn a_confirmed_merge_points_the_checkout_at_the_target() {
        let (source, target) = (ProjectId::new(), ProjectId::new());
        let by_name = MergeProjectRequest {
            workspace: "default".into(),
            from: Some("acme-api".into()),
            from_project_id: None,
            into: Some("api".into()),
            into_project_id: None,
            confirm: true,
        };
        let report = serde_json::json!({
            "from": "default/acme-api",
            "into": "default/api",
            "from_project_id": source.to_string(),
            "into_project_id": target.to_string(),
        });
        assert_eq!(
            after_merge(source, &by_name, &report),
            (target, "api".to_owned())
        );

        // A server that predates the id fields: the ids the operator passed.
        let by_id = MergeProjectRequest {
            workspace: "default".into(),
            from: None,
            from_project_id: Some(source.to_string()),
            into: None,
            into_project_id: Some(target.to_string()),
            confirm: true,
        };
        let names_only = serde_json::json!({
            "from": "default/acme-api",
            "into": "default/api",
        });
        assert_eq!(
            after_merge(source, &by_id, &names_only),
            (target, "api".to_owned())
        );

        // With neither, the binding cannot be re-pointed: only the link follows.
        assert_eq!(
            after_merge(source, &by_name, &names_only),
            (source, "api".to_owned())
        );
    }
}
