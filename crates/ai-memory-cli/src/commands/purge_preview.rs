//! Shared best-effort dry-run preview machinery for `purge-project` and
//! `purge-session`.
//!
//! Both subcommands refuse without `--confirm`, but first ask the server for
//! a bounded preview (`"dry_run": true`, which wins over `confirm`
//! server-side) of what the confirmed run would do. The HTTP round trip, its
//! timeout, and the classification of the result into "print this" vs "stay
//! silent" are identical between the two commands — only the request body's
//! shape, the endpoint path, and the summary line's wording differ, so those
//! stay in `purge_project.rs` / `purge_session.rs` and this module holds only
//! what would otherwise be copy-pasted between them.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerResponseError, post_json};

/// How long the preview request (auth resolution plus the HTTP round trip)
/// is allowed to take before this falls back to the plain refusal. The
/// preview is optional information layered on top of a refusal that must
/// still happen either way, so it must never be the reason `--confirm` takes
/// noticeably longer than it used to.
pub const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);

/// What became of the best-effort preview request, reduced to what deciding
/// whether — and what — to print needs. Kept separate from the network call
/// itself so the printing decision is a pure function and testable without a
/// server.
pub enum PreviewOutcome {
    /// A 200 with `"dry_run": true`: the server understood the request and
    /// ran the preview.
    Previewed(serde_json::Value),
    /// A 200 without `dry_run` set: an old-enough server both predates the
    /// field AND happens to 200 an unrecognized shape. Treated the same as
    /// not getting a preview at all.
    Ignored,
    /// A non-2xx response with a body worth showing: the scope resolved to
    /// something the operator should know about before the refusal (a 404
    /// naming the missing project/session, a 409 naming a live managed run,
    /// a 403 naming the auth problem), or an unexpected status this command
    /// has no specific handling for.
    Refused { status: u16, body: String },
    /// The request predates `dry_run` support (400, the pre-existing
    /// "confirm=true" refusal body) — not worth repeating, since `run` below
    /// prints its own version of exactly that message next regardless.
    OlderServer,
    /// Timed out or never reached a server at all (DNS/connect failure,
    /// auth-refresh hang, etc). Indistinguishable from the operator's
    /// perspective, and neither is this command's business to diagnose.
    Unreachable,
}

/// Run the preview request under [`PREVIEW_TIMEOUT`] and classify the
/// result.
///
/// The preview goes to the server the confirmed purge would reach:
/// [`ServerEndpoint::for_checkout`] picks the server profile this checkout's
/// marker selects, with that profile's own token, and the install's server
/// otherwise (#2016). Auth resolution (which can itself refresh an OIDC token
/// over the network) runs inside the same timeout so a hung refresh cannot
/// silently make a `--confirm`-less purge command block far longer than the
/// rest of it ever has.
///
/// # Errors
/// A marker that selects a server profile which does not resolve. The
/// confirmed purge fails closed on it, and so does the preview: it is sent
/// nowhere rather than to the install's server with this checkout's names.
pub async fn run_preview<Req: Serialize>(
    config: &Config,
    path: &str,
    request: &Req,
) -> Result<PreviewOutcome> {
    let attempt = tokio::time::timeout(PREVIEW_TIMEOUT, async {
        let endpoint = ServerEndpoint::for_checkout(config).await?;
        Ok::<_, anyhow::Error>(post_json::<_, serde_json::Value>(&endpoint, path, request).await)
    })
    .await;

    let Ok(routed) = attempt else {
        return Ok(PreviewOutcome::Unreachable);
    };
    Ok(match routed? {
        Ok(report) if report["dry_run"].as_bool().unwrap_or(false) => {
            PreviewOutcome::Previewed(report)
        }
        Ok(_) => PreviewOutcome::Ignored,
        Err(e) => match e.downcast_ref::<ServerResponseError>() {
            Some(resp) if resp.status().as_u16() == 400 => PreviewOutcome::OlderServer,
            Some(resp) => PreviewOutcome::Refused {
                status: resp.status().as_u16(),
                body: resp.body().to_string(),
            },
            // Not an HTTP response at all: connect/DNS failure, request
            // timeout already handled above, or a body that failed to
            // deserialize as JSON.
            None => PreviewOutcome::Unreachable,
        },
    })
}

/// Format a `PreviewOutcome::Refused` body as the one line every caller
/// prints before its refusal: `Preview refused (<status>): <message>`. The
/// message is the server's own `{"error": ...}` string when the body parses
/// that way, and the raw body otherwise. Both are sanitized for terminal output.
pub fn refused_message(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    format!(
        "Preview refused ({status}): {}",
        super::show::terminal_text(&message)
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// One fixture server: answers `path` with `body` and records the
    /// `Authorization` header of every request it receives.
    async fn recording_server(
        path: &'static str,
        body: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let app = axum::Router::new().route(
            path,
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let record = record.clone();
                async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    record.lock().unwrap().push(auth);
                    axum::Json(serde_json::from_str::<serde_json::Value>(body).unwrap())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen)
    }

    /// A checkout under `<home>/work/team-b` whose marker is `marker`, a
    /// `team-b` profile pointing at `profile_url`, and a config whose own
    /// server is `install_url` with its own bearer.
    fn profiled_checkout(
        tmp: &tempfile::TempDir,
        marker: &str,
        profile_url: &str,
        install_url: &str,
    ) -> Config {
        let home = tmp.path().join("home");
        let root = home.join("work").join("team-b");
        let repo = root.join("repo");
        let data_dir = home.join("data");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(repo.join(".ai-memory.toml"), marker).unwrap();
        let name = crate::server_profiles::ProfileName::parse("team-b").unwrap();
        crate::server_profiles::add(
            &data_dir,
            &name,
            profile_url,
            &[root.to_string_lossy().into_owned()],
            Some("PROFILE-TOKEN"),
        )
        .unwrap();
        let mut config = Config {
            data_dir,
            server_url: install_url.to_owned(),
            runtime_env: crate::config::RuntimeEnv::for_checkout_tests(
                repo.to_string_lossy(),
                home.to_string_lossy(),
                None,
                false,
            ),
            ..Config::default()
        };
        config.auth.bearer_token = Some("INSTALL-TOKEN".to_owned());
        config
    }

    const PREVIEW: &str = r#"{"label": "team-b/repo", "pages_deleted": 1, "dry_run": true}"#;
    const WRONG_SERVER: &str = r#"{"label": "team-b/repo", "pages_deleted": 3, "dry_run": true}"#;

    /// #2016 routes the confirmed purge to the server profile the checkout's
    /// marker selects; the preview must ask that same server, with that
    /// profile's token, and never show the install's counts instead.
    #[tokio::test]
    async fn a_profiled_checkout_previews_on_the_profile_server() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (profile_url, profile_seen) = recording_server("/admin/purge-project", PREVIEW).await;
        let (install_url, install_seen) =
            recording_server("/admin/purge-project", WRONG_SERVER).await;
        let config = profiled_checkout(&tmp, "server = \"team-b\"\n", &profile_url, &install_url);

        let outcome = run_preview(
            &config,
            "/admin/purge-project",
            &serde_json::json!({"workspace": "team-b", "project": "repo", "dry_run": true}),
        )
        .await
        .unwrap();

        match outcome {
            PreviewOutcome::Previewed(report) => assert_eq!(report["pages_deleted"], 1),
            _ => panic!("expected the profile server's preview"),
        }
        assert_eq!(*profile_seen.lock().unwrap(), ["Bearer PROFILE-TOKEN"]);
        assert!(
            install_seen.lock().unwrap().is_empty(),
            "the install's server must not be asked about this checkout"
        );
    }

    /// A marker whose profile does not resolve stops the preview before
    /// anything is sent, as it stops the confirmed purge; falling back to the
    /// install's server would preview, and name this checkout to, a server
    /// the purge never touches.
    #[tokio::test]
    async fn a_refused_profile_fails_the_preview_without_sending_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (profile_url, profile_seen) = recording_server("/admin/purge-session", PREVIEW).await;
        let (install_url, install_seen) =
            recording_server("/admin/purge-session", WRONG_SERVER).await;
        let config = profiled_checkout(&tmp, "server = \"nobody\"\n", &profile_url, &install_url);

        let Err(error) = run_preview(
            &config,
            "/admin/purge-session",
            &serde_json::json!({"workspace": "team-b", "project": "repo", "dry_run": true}),
        )
        .await
        else {
            panic!("a refused profile must fail the preview");
        };
        let error = error.to_string();

        assert!(error.contains("rejected-unknown-profile"), "{error}");
        assert!(!error.contains("TOKEN"), "{error}");
        assert!(profile_seen.lock().unwrap().is_empty());
        assert!(install_seen.lock().unwrap().is_empty());
    }
}
