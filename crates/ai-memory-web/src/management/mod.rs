//! Optional askama management pages. Forms delegate to authenticated engine routers.
//! No network proxy, database writes, JavaScript framework, or new auth implementation.
use askama::Template;
use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{Request, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
mod backend;
mod forms;
mod views;
use backend::Client;
use views::{ConsoleView, Identity, Screen};

// UI policy counts Unicode code points; the engine keeps its UTF-8 byte policy.
const MIN_PASSWORD_CHARACTERS: usize = 12;
const SCRIPT: &str = include_str!("../../static/management/theme.js");
#[derive(Clone)]
struct Management {
    backend: Router,
    root: String,
}

/// Build the optional HTML console around existing auth/admin/management routers.
/// The injected backend uses its original middleware for every request. The
/// caller mounts this only when the runtime console switch is enabled.
pub fn router(base_path: &str, backend: Router) -> Router {
    let root = format!("{}/manage", crate::mount::normalize_prefix(base_path));
    crate::localization::apply(
        Router::new()
            .route("/manage", get(page))
            .route("/manage/", get(page))
            .route("/manage/{screen}", get(page))
            .route("/manage/action", post(forms::submit))
            .route(
                "/manage/static/tailwind.css",
                get(crate::routes::statics::tailwind_css),
            )
            .route("/manage/static/logo.png", get(crate::routes::statics::logo))
            .route("/manage/static/{asset}", get(asset))
            .with_state(Arc::new(Management { backend, root }))
            .layer(axum::middleware::from_fn(secure)),
    )
}

#[derive(Default, Deserialize)]
struct Selection {
    workspace: Option<String>,
    project: Option<String>,
    session: Option<ai_memory_core::SessionId>,
    #[serde(default)]
    offset: usize,
}
async fn page(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<Management>>,
    Query(selection): Query<Selection>,
    request: Request<Body>,
) -> Response {
    let name = request.uri().path().rsplit('/').next().unwrap_or("");
    let screen = match name {
        "" | "manage" | "sessions" => Screen::Sessions,
        "login" => Screen::Login,
        "password" => Screen::Password,
        "users" => Screen::Users,
        "keys" => Screen::Keys,
        "session" => Screen::Session,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let (parts, _) = request.into_parts();
    let client = Client {
        router: state.backend.clone(),
        headers: parts.headers,
        extensions: parts.extensions,
    };
    let mut view = ConsoleView::new(&state.root, screen, backend::csrf_cookie(&client.headers));
    view.i18n = i18n.clone();
    if screen != Screen::Login {
        let me = match identity(&client, &state.root).await {
            Ok(me) => me,
            Err(response) => return response,
        };
        if me.must_change_password && screen != Screen::Password {
            return Redirect::to(&format!("{}/password", state.root)).into_response();
        }
        view.username = me.username.clone();
        view.admin = me.admin;
        if screen == Screen::Users && !me.admin {
            return failure(&i18n, StatusCode::FORBIDDEN, "Admin access required".into());
        }
        if let Err(response) = view.load(&client, &me, selection).await {
            return response;
        }
    }
    render(view)
}
async fn identity(client: &Client, root: &str) -> Result<Identity, Response> {
    let reply = client.call("GET", "/auth/me", None, None).await?;
    let username = views::text(&reply.data, "username");
    if !reply.status.is_success() || username.is_empty() {
        return Err(Redirect::to(&format!("{root}/login")).into_response());
    }
    Ok(Identity {
        username,
        admin: reply.data["capabilities"]["user_management"] == true,
        must_change_password: reply.data["must_change_password"] == true,
    })
}
fn render(view: impl Template) -> Response {
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::warn!(%error, "management template failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
#[derive(Template)]
#[template(path = "management/error.html")]
struct ErrorView {
    pub i18n: ai_memory_core::i18n::Localizer,
    message: String,
}
fn failure(
    i18n: &ai_memory_core::i18n::Localizer,
    status: StatusCode,
    message: String,
) -> Response {
    let mut response = render(ErrorView {
        i18n: i18n.clone(),
        message: i18n.native_error(&message),
    });
    *response.status_mut() = status;
    response
}
async fn secure(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let digest =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(SCRIPT.as_bytes()));
    let csp = format!(
        "default-src 'self'; script-src 'sha256-{digest}'; style-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
    );
    if let Ok(value) = csp.parse() {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}
async fn asset(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let (mime, body): (&str, &[u8]) = match name.as_str() {
        "instrument-sans-latin-wdth-normal.woff2" => (
            "font/woff2",
            include_bytes!("../../static/management/instrument-sans-latin-wdth-normal.woff2"),
        ),
        "bricolage-grotesque-latin-standard-normal.woff2" => (
            "font/woff2",
            include_bytes!(
                "../../static/management/bricolage-grotesque-latin-standard-normal.woff2"
            ),
        ),
        "commit-mono-latin-400-normal.woff2" => (
            "font/woff2",
            include_bytes!("../../static/management/commit-mono-latin-400-normal.woff2"),
        ),
        "font-licenses.txt" => (
            "text/plain; charset=utf-8",
            include_bytes!("../../static/management/font-licenses.txt"),
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, mime)], body).into_response()
}
