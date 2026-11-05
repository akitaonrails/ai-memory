//! Repair for a session captured under the checkout's old folder-name project
//! (#1993).
//!
//! A repository that gained its git remote after capture began has two
//! projects: the folder-named one that holds its sessions and the
//! remote-derived one `ai-memory run` now resolves. Resuming such a session
//! cannot link it to the run. When the remote-derived project is still empty,
//! renaming the folder-named project to the remote-derived name joins them
//! without losing anything: a rename keeps the project id, so sessions,
//! observations and pages follow. The empty remote-derived project is renamed
//! aside first, never deleted.

use std::fmt;
use std::io;

use anyhow::Result;
use serde::Deserialize;

use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerResponseError, post_json};

/// Server `code` for a link refused because another project holds the session.
const NATIVE_SESSION_IN_OTHER_PROJECT: &str = ai_memory_hooks::NATIVE_SESSION_IN_OTHER_PROJECT;

/// A resumed native session held by another project of the same operator.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ProjectSplit {
    workspace: String,
    session_project: String,
    session_project_id: String,
    run_workspace: String,
    run_project: String,
    run_project_id: String,
    run_legacy_name: Option<String>,
    run_sessions: u64,
    run_pages: u64,
}

impl ProjectSplit {
    /// The split behind a refused link, when the server reported one.
    pub(crate) fn from_error(error: &anyhow::Error) -> Option<Self> {
        let response = error.downcast_ref::<ServerResponseError>()?;
        if response.status() != reqwest::StatusCode::CONFLICT {
            return None;
        }
        let body: serde_json::Value = serde_json::from_str(response.body()).ok()?;
        if body["code"].as_str() != Some(NATIVE_SESSION_IN_OTHER_PROJECT) {
            return None;
        }
        serde_json::from_value(body).ok()
    }

    /// Whether renaming joins the two halves without hiding anything: the
    /// session sits in the run project's own old folder-name project, in the
    /// same workspace, and the remote-derived project has recorded nothing.
    fn repairable(&self) -> bool {
        self.workspace == self.run_workspace
            && self.run_legacy_name.as_deref() == Some(self.session_project.as_str())
            && self.run_sessions == 0
            && self.run_pages == 0
    }

    fn archive_name(&self, day: &str) -> String {
        format!("{}-split-{day}", self.run_project)
    }

    /// The two renames, in order, and whether each re-keys the client-local
    /// checkout links. Only the second does: links naming the old folder
    /// project follow it to the remote-derived name, while links already on
    /// that name must stay there rather than follow the empty project aside.
    fn renames(&self, day: &str) -> [(&str, String, bool); 2] {
        [
            (self.run_project_id.as_str(), self.archive_name(day), false),
            (
                self.session_project_id.as_str(),
                self.run_project.clone(),
                true,
            ),
        ]
    }

    fn commands(&self, day: &str) -> String {
        self.renames(day)
            .iter()
            .map(|(id, to, _)| {
                format!(
                    "  ai-memory rename-project --workspace {} --project-id {id} --to {to}",
                    self.run_workspace
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl fmt::Display for ProjectSplit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the session being resumed was recorded in project {}/{}, but this checkout now \
             resolves to {}/{}; the agent was not started",
            self.workspace, self.session_project, self.run_workspace, self.run_project
        )?;
        if self.repairable() {
            write!(
                f,
                ". Join them by renaming (nothing is deleted), then run again:\n{}",
                self.commands(&today())
            )
        } else {
            write!(
                f,
                ". {}/{} already holds its own sessions or pages, so it was not renamed \
                 automatically; see docs/lifecycle-ops.md before merging the two",
                self.run_workspace, self.run_project
            )
        }
    }
}

impl std::error::Error for ProjectSplit {}

fn today() -> String {
    jiff::Zoned::now().strftime("%Y%m%d").to_string()
}

/// Ask on the terminal whether to apply the repair, defaulting to yes.
///
/// Returns `false` without asking when the split is not safely repairable.
pub(crate) fn confirm_repair(
    i18n: &ai_memory_core::i18n::Localizer,
    split: &ProjectSplit,
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> io::Result<bool> {
    if !split.repairable() {
        return Ok(false);
    }
    writeln!(
        output,
        "{}",
        i18n.format(
            "cli-run-project-split-explain",
            &[
                ("workspace", split.workspace.clone()),
                ("old", split.session_project.clone()),
                ("new", split.run_project.clone()),
                ("archive", split.archive_name(&today())),
            ]
        )
    )?;
    write!(output, "{}", i18n.text("cli-run-project-split-prompt"))?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(false);
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes" | "s" | "sim"
    ))
}

/// Apply both renames through the server's rename route, re-keying the
/// client-local checkout links as [`ProjectSplit::renames`] says.
pub(crate) async fn apply_repair(config: &Config, split: &ProjectSplit) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let day = today();
    for (project_id, to, rekey) in split.renames(&day) {
        let body = serde_json::json!({
            "workspace": split.run_workspace,
            "project_id": project_id,
            "to": to,
        });
        let summary: serde_json::Value =
            post_json(&endpoint, "/admin/rename-project", &body).await?;
        if rekey && let Some(from) = summary["from"].as_str() {
            // The checkout link is a client-side convenience; the server
            // rename is what joins the projects.
            let _ = super::project_registry::rekey_scope(
                config,
                &endpoint,
                &split.run_workspace,
                from,
                &split.run_workspace,
                &to,
            );
        }
    }
    eprintln!(
        "{}",
        config.i18n.format(
            "cli-run-project-split-repaired",
            &[
                ("workspace", split.run_workspace.clone()),
                ("old", split.session_project.clone()),
                ("new", split.run_project.clone()),
            ]
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split() -> ProjectSplit {
        ProjectSplit {
            workspace: "default".into(),
            session_project: "api".into(),
            session_project_id: "01a19c51-63d5-7dc2-8dc1-90201095ba96".into(),
            run_workspace: "default".into(),
            run_project: "acme-api".into(),
            run_project_id: "01a19da2-094f-7830-9807-1d32f2fdb464".into(),
            run_legacy_name: Some("api".into()),
            run_sessions: 0,
            run_pages: 0,
        }
    }

    fn conflict(body: serde_json::Value) -> anyhow::Error {
        crate::http_client::server_response_error_for_test(
            reqwest::Method::POST,
            "/workstream/runs/x/link",
            reqwest::StatusCode::CONFLICT,
            body.to_string(),
        )
    }

    #[test]
    fn a_split_conflict_is_recognized_through_the_error_chain() {
        let body = serde_json::json!({
            "error": "native session belongs to project default/api, not default/acme-api",
            "code": NATIVE_SESSION_IN_OTHER_PROJECT,
            "workspace": "default",
            "session_project": "api",
            "session_project_id": "01a19c51-63d5-7dc2-8dc1-90201095ba96",
            "run_workspace": "default",
            "run_project": "acme-api",
            "run_project_id": "01a19da2-094f-7830-9807-1d32f2fdb464",
            "run_legacy_name": "api",
            "run_sessions": 0,
            "run_pages": 0,
        });
        let error = conflict(body).context("linking the managed native session");
        assert_eq!(ProjectSplit::from_error(&error), Some(split()));
    }

    #[test]
    fn other_conflicts_are_not_a_split() {
        let generic = conflict(serde_json::json!({"error": "managed run is not active"}));
        assert_eq!(ProjectSplit::from_error(&generic), None);
        let not_found = crate::http_client::server_response_error_for_test(
            reqwest::Method::POST,
            "/workstream/runs/x/link",
            reqwest::StatusCode::NOT_FOUND,
            serde_json::json!({"code": NATIVE_SESSION_IN_OTHER_PROJECT}).to_string(),
        );
        assert_eq!(ProjectSplit::from_error(&not_found), None);
    }

    #[test]
    fn only_the_folder_name_split_into_an_empty_project_is_repairable() {
        assert!(split().repairable());
        let with_sessions = ProjectSplit {
            run_sessions: 1,
            ..split()
        };
        let with_pages = ProjectSplit {
            run_pages: 2,
            ..split()
        };
        let unrelated = ProjectSplit {
            run_legacy_name: Some("other".into()),
            ..split()
        };
        let no_legacy = ProjectSplit {
            run_legacy_name: None,
            ..split()
        };
        let other_workspace = ProjectSplit {
            workspace: "team".into(),
            ..split()
        };
        for case in [
            with_sessions,
            with_pages,
            unrelated,
            no_legacy,
            other_workspace,
        ] {
            assert!(!case.repairable(), "{case:?}");
        }
    }

    /// The empty project is renamed aside first, without moving the
    /// checkout links that already name it; only the folder project's
    /// links follow its rename.
    #[test]
    fn the_repair_archives_the_empty_project_before_taking_its_name() {
        let split = split();
        assert_eq!(
            split.renames("20261105"),
            [
                (
                    "01a19da2-094f-7830-9807-1d32f2fdb464",
                    "acme-api-split-20261105".to_owned(),
                    false
                ),
                (
                    "01a19c51-63d5-7dc2-8dc1-90201095ba96",
                    "acme-api".to_owned(),
                    true
                ),
            ]
        );
    }

    #[test]
    fn the_error_prints_the_commands_only_when_the_repair_is_safe() {
        let repairable = split().to_string();
        assert!(
            repairable.contains("--project-id 01a19c51-63d5-7dc2-8dc1-90201095ba96 --to acme-api")
        );
        assert!(repairable.contains("nothing is deleted"));
        let busy = ProjectSplit {
            run_sessions: 3,
            ..split()
        }
        .to_string();
        assert!(!busy.contains("rename-project"));
        assert!(busy.contains("already holds its own sessions or pages"));
    }

    #[test]
    fn the_prompt_defaults_to_yes_and_accepts_no() {
        let i18n = ai_memory_core::i18n::Localizer::default();
        for (answer, expected) in [("\n", true), ("y\n", true), ("n\n", false), ("no\n", false)] {
            let mut output = Vec::new();
            let decided =
                confirm_repair(&i18n, &split(), &mut answer.as_bytes(), &mut output).unwrap();
            assert_eq!(decided, expected, "{answer:?}");
            let printed = String::from_utf8(output).unwrap();
            assert!(printed.contains("acme-api-split-"), "{printed}");
        }
    }

    #[test]
    fn eof_does_not_confirm_the_repair() {
        let i18n = ai_memory_core::i18n::Localizer::default();
        let mut output = Vec::new();
        assert!(
            !confirm_repair(&i18n, &split(), &mut io::empty(), &mut output).unwrap(),
            "closing the prompt without an answer must not rename either project"
        );
        assert!(!output.is_empty(), "the repair was offered before EOF");
    }

    #[test]
    fn an_unsafe_split_is_never_offered() {
        let i18n = ai_memory_core::i18n::Localizer::default();
        let mut output = Vec::new();
        let busy = ProjectSplit {
            run_pages: 1,
            ..split()
        };
        assert!(!confirm_repair(&i18n, &busy, &mut "y\n".as_bytes(), &mut output).unwrap());
        assert!(output.is_empty());
    }
}
