//! Public builtin auth pages (`/login`, `/change-password`).

use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::html_auth::{HtmlAuthRedirectConfig, sanitize_next};
use crate::templates::{ChangePasswordView, LoginView};

#[derive(Debug, Deserialize)]
pub(crate) struct NextQuery {
    #[serde(default)]
    next: Option<String>,
}

pub(crate) async fn login(
    State(cfg): State<Arc<HtmlAuthRedirectConfig>>,
    Query(q): Query<NextQuery>,
) -> Response {
    let next = sanitize_next(q.next.as_deref(), &cfg.web_root);
    let view = LoginView {
        next,
        change_password_href: cfg.change_password_path.clone(),
    };
    render(view)
}

pub(crate) async fn change_password(
    State(cfg): State<Arc<HtmlAuthRedirectConfig>>,
    Query(q): Query<NextQuery>,
) -> Response {
    let next = sanitize_next(q.next.as_deref(), &cfg.web_root);
    let view = ChangePasswordView { next };
    render(view)
}

fn render(view: impl Template) -> Response {
    match view.render() {
        Ok(body) => Html(body).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "template render error\n",
        )
            .into_response(),
    }
}
