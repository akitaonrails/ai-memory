//! Plain POST form adapters: fixed targets, existing session/CSRF and capability gates.
use super::{
    Management,
    backend::{Client, copy_cookies},
    views::{ConsoleView, Screen, text},
};
use ai_memory_core::{ApiCredentialId, SessionId};
use axum::{
    Form,
    body::Body,
    extract::{FromRequest, State},
    http::{Request, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Action {
    Login,
    Password,
    Logout,
    AddUser,
    Role,
    ResetPassword,
    Disable,
    Enable,
    IssueKey,
    RevokeAdminKey,
    CreateKey,
    RevokeKey,
    PreviewPurge,
    Purge,
}
#[derive(Deserialize)]
struct Command {
    action: Action,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    new_password_confirmation: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    label: String,
    session: Option<SessionId>,
    credential: Option<ApiCredentialId>,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    confirm: bool,
}
pub(super) async fn submit(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<Management>>,
    request: Request<Body>,
) -> Response {
    // Plain login forms lack the JSON/preflight barrier used by /auth/login.
    // Require a same-origin POST before translating any form into JSON.
    if !same_origin(request.headers()) {
        return super::failure(
            &i18n,
            StatusCode::FORBIDDEN,
            "Cross-origin form request refused".into(),
        );
    }
    let headers = request.headers().clone();
    let extensions = request.extensions().clone();
    let Form(command) = match Form::<Command>::from_request(request, &()).await {
        Ok(form) => form,
        Err(error) => return error.into_response(),
    };
    let client = Client {
        router: state.backend.clone(),
        headers,
        extensions,
    };
    let mut view = ConsoleView::new(&state.root, Screen::Secret, command.csrf.clone());
    view.i18n = i18n.clone();
    if command.action != Action::Login {
        let me = match super::identity(&client, &state.root).await {
            Ok(me) => me,
            Err(response) => return response,
        };
        if me.must_change_password
            && command.action != Action::Password
            && command.action != Action::Logout
        {
            return super::failure(
                &i18n,
                StatusCode::FORBIDDEN,
                "Password change required".into(),
            );
        }
        view.username = me.username;
        view.admin = me.admin;
    }
    if command.action == Action::Password
        && command.new_password.chars().count() < super::MIN_PASSWORD_CHARACTERS
    {
        return super::failure(
            &i18n,
            StatusCode::BAD_REQUEST,
            "Use at least 12 characters.".into(),
        );
    }
    let (method, path, body, destination) = match command.target() {
        Ok(target) => target,
        Err(message) => return super::failure(&i18n, StatusCode::BAD_REQUEST, message.to_string()),
    };
    let reply = match client
        .call(
            method,
            &path,
            Some(body),
            if command.action == Action::Login {
                None
            } else {
                Some(&command.csrf)
            },
        )
        .await
    {
        Ok(reply) => reply,
        Err(response) => return response,
    };
    let mut response = if !reply.status.is_success() {
        super::failure(&i18n, reply.status, text(&reply.data, "error"))
    } else if command.action == Action::PreviewPurge {
        view.preview(
            &reply.data,
            command.session.map(|s| s.to_string()).unwrap_or_default(),
            command.workspace,
            command.project,
        );
        super::render(view)
    } else if !text(&reply.data, "temporary_password").is_empty()
        || !text(&reply.data, "token").is_empty()
    {
        view.secret_title = i18n.text(if reply.data["temporary_password"].is_string() {
            "web-temporary-password"
        } else {
            "web-new-mcp-key"
        });
        view.secret = if reply.data["temporary_password"].is_string() {
            text(&reply.data, "temporary_password")
        } else {
            text(&reply.data, "token")
        };
        super::render(view)
    } else if command.action == Action::Purge
        && reply.data["files_failed"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    {
        super::failure(&i18n, StatusCode::OK,"Database deletion completed, but some wiki files could not be removed. Ask an Admin to check the server logs.".into())
    } else {
        let destination =
            if command.action == Action::Login && reply.data["must_change_password"] == true {
                "password".to_string()
            } else {
                destination
            };
        Redirect::to(&format!("{}/{destination}", state.root)).into_response()
    };
    copy_cookies(&reply.headers, &mut response);
    response
}
impl Command {
    fn target(&self) -> Result<(&'static str, String, serde_json::Value, String), &'static str> {
        let user = crate::templates::encode_segment(&self.username);
        let key = || self.credential.ok_or("Credential ID required");
        let session = || self.session.ok_or("Session ID required");
        Ok(match self.action {
            Action::Login => (
                "POST",
                "/auth/login".into(),
                json!({"username":self.username,"password":self.password}),
                "sessions".into(),
            ),
            Action::Password => (
                "POST",
                "/auth/password".into(),
                json!({"current_password":self.current_password,"new_password":self.new_password,"new_password_confirmation":self.new_password_confirmation}),
                "sessions".into(),
            ),
            Action::Logout => ("POST", "/auth/logout".into(), json!({}), "login".into()),
            Action::AddUser => (
                "POST",
                "/admin/human-users".into(),
                json!({"username":self.username,"name":if self.name.is_empty() { None } else { Some(&self.name) },"role":self.role}),
                "users".into(),
            ),
            Action::Role => (
                "PATCH",
                format!("/admin/users/{user}"),
                json!({"role":self.role}),
                "users".into(),
            ),
            Action::Disable | Action::Enable | Action::ResetPassword => {
                let action = match self.action {
                    Action::Disable => "disable",
                    Action::Enable => "enable",
                    _ => "reset-password",
                };
                (
                    "POST",
                    format!("/admin/users/{user}/{action}"),
                    json!({}),
                    "users".into(),
                )
            }
            Action::IssueKey => (
                "POST",
                "/admin/api-credentials".into(),
                json!({"username":self.username,"label":self.label}),
                "users".into(),
            ),
            Action::RevokeAdminKey => (
                "POST",
                format!("/admin/api-credentials/{}/revoke", key()?),
                json!({}),
                "users".into(),
            ),
            Action::CreateKey => (
                "POST",
                "/management/v1/credentials".into(),
                json!({"label":self.label}),
                "keys".into(),
            ),
            Action::RevokeKey => (
                "POST",
                format!("/management/v1/credentials/{}/revoke", key()?),
                json!({}),
                "keys".into(),
            ),
            Action::PreviewPurge | Action::Purge => (
                "POST",
                format!("/management/v1/sessions/{}/purge", session()?),
                json!({"workspace":self.workspace,"project":self.project,"dry_run":self.action==Action::PreviewPurge,"confirm":self.confirm}),
                format!(
                    "sessions?{}",
                    super::views::query(&self.workspace, &self.project, 0)
                ),
            ),
        })
    }
}

fn same_origin(headers: &axum::http::HeaderMap) -> bool {
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::Uri>().ok());
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let origin_matches = origin.is_some_and(|origin| {
        matches!(origin.scheme_str(), Some("http" | "https"))
            && origin.path() == "/"
            && origin.query().is_none()
            && origin
                .authority()
                .zip(host)
                .is_some_and(|(authority, host)| authority.as_str().eq_ignore_ascii_case(host))
    });
    let fetch_site_matches = headers.get("sec-fetch-site").is_none_or(|site| {
        site.to_str().is_ok_and(|site| {
            site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none")
        })
    });
    origin_matches && fetch_site_matches
}
