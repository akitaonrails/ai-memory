//! HTML auth redirect helpers for the builtin wiki browser.
//!
//! After [`require_dual_auth`](ai_memory_mcp::require_dual_auth) returns
//! JSON 401/403, navigational browser GETs to the wiki should land on the
//! builtin login / change-password pages instead. `/api/v1` stays JSON.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::mount::normalize_prefix;

/// Paths and prefixes the HTML auth redirect middleware needs.
#[derive(Clone, Debug)]
pub struct HtmlAuthRedirectConfig {
    /// Normalised base path (`""` or `/wiki`).
    pub base_path: String,
    /// External wiki root (`""`, `/web`, or `/wiki/web`).
    pub web_root: String,
    /// External login path (`/web/login` or `/login` when mounted at root).
    pub login_path: String,
    /// External change-password path.
    pub change_password_path: String,
}

impl HtmlAuthRedirectConfig {
    /// Build redirect targets from the same base/slug the mount uses.
    #[must_use]
    pub fn from_mount(base_path: &str, web_slug: &str) -> Self {
        let base_path = normalize_prefix(base_path);
        let web_root = web_root(&base_path, web_slug);
        let login_path = join_root(&web_root, "login");
        let change_password_path = join_root(&web_root, "change-password");
        Self {
            base_path,
            web_root,
            login_path,
            change_password_path,
        }
    }
}

/// `{base_path}{web_slug}` with both sides normalised; empty when both are root.
#[must_use]
pub(crate) fn web_root(base_path: &str, web_slug: &str) -> String {
    format!(
        "{}{}",
        normalize_prefix(base_path),
        normalize_prefix(web_slug)
    )
}

fn join_root(web_root: &str, leaf: &str) -> String {
    if web_root.is_empty() {
        format!("/{leaf}")
    } else {
        format!("{web_root}/{leaf}")
    }
}

/// Response marker for a 403 that means "this page is root-only".
///
/// [`html_auth_redirect_mw`] sends every other HTML 403 to the
/// change-password form; a signed-in non-root user must see the refusal
/// instead.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AdminRequired;

/// Allow only same-origin relative paths under `web_root`.
///
/// Rejects protocol-relative `//`, absolute URLs (`http:` / `https:`),
/// backslashes, and paths outside the wiki slug. Default is the wiki root.
#[must_use]
pub fn sanitize_next(raw: Option<&str>, web_root: &str) -> String {
    let default = if web_root.is_empty() {
        "/".to_string()
    } else {
        web_root.to_string()
    };
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return default;
    };
    if raw.contains("://") || raw.starts_with("//") || raw.contains('\\') || raw.contains('\0') {
        return default;
    }
    if !raw.starts_with('/') {
        return default;
    }
    let path = raw.split_once(['?', '#']).map(|(p, _)| p).unwrap_or(raw);
    if web_root.is_empty() {
        // Wiki mounted at host/base root — still reject escaping to
        // sibling host routes by requiring a single-path absolute form
        // without `..` segments.
        if path_has_dot_segment(path) {
            return default;
        }
        return raw.to_string();
    }
    if path == web_root || path.starts_with(&format!("{web_root}/")) {
        if path_has_dot_segment(path) {
            return default;
        }
        return raw.to_string();
    }
    default
}

fn path_has_dot_segment(path: &str) -> bool {
    path.split('/').any(|s| s == "." || s == "..")
}

/// Map JSON 401/403 on HTML navigational wiki GETs to the builtin auth pages.
pub async fn html_auth_redirect_mw(
    State(cfg): State<Arc<HtmlAuthRedirectConfig>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let wants_html = is_html_navigation(&req);
    let path = req.uri().path().to_string();
    let full_path_and_query = {
        let path = req.uri().path();
        let q = req.uri().query();
        // Reconstruct the external path: middleware runs inside the
        // base-path nest, so prepend `base_path` for Location / next.
        let external = if cfg.base_path.is_empty() {
            path.to_string()
        } else if path == "/" {
            cfg.base_path.clone()
        } else {
            format!("{}{path}", cfg.base_path)
        };
        match q {
            Some(q) if !q.is_empty() => format!("{external}?{q}"),
            _ => external,
        }
    };
    let is_api = is_api_v1_path(&path);

    let resp = next.run(req).await;
    if !wants_html || is_api {
        return resp;
    }
    match resp.status() {
        StatusCode::UNAUTHORIZED => {
            let next_q =
                urlencoding_encode(&sanitize_next(Some(&full_path_and_query), &cfg.web_root));
            Redirect::to(&format!("{}?next={next_q}", cfg.login_path)).into_response()
        }
        StatusCode::FORBIDDEN if resp.extensions().get::<AdminRequired>().is_none() => {
            Redirect::to(&cfg.change_password_path).into_response()
        }
        _ => resp,
    }
}

fn is_api_v1_path(path: &str) -> bool {
    path == "/api/v1" || path.starts_with("/api/v1/")
}

fn is_html_navigation(req: &Request<axum::body::Body>) -> bool {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return false;
    }
    if let Some(dest) = req
        .headers()
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
    {
        return dest.eq_ignore_ascii_case("document");
    }
    req.headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/html"))
}

/// Minimal application/x-www-form-urlencoded encoder for the `next` query value.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b'/' => out.push_str("%2F"),
            b'?' => out.push_str("%3F"),
            b'=' => out.push_str("%3D"),
            b'&' => out.push_str("%26"),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    #[test]
    fn sanitize_next_defaults_and_rejects_open_redirects() {
        assert_eq!(sanitize_next(None, "/web"), "/web");
        assert_eq!(sanitize_next(Some(""), "/web"), "/web");
        assert_eq!(sanitize_next(Some("/web/w/a"), "/web"), "/web/w/a");
        assert_eq!(
            sanitize_next(Some("/web/search?q=x"), "/web"),
            "/web/search?q=x"
        );
        assert_eq!(sanitize_next(Some("//evil.example"), "/web"), "/web");
        assert_eq!(sanitize_next(Some("https://evil.example/"), "/web"), "/web");
        assert_eq!(sanitize_next(Some("/other"), "/web"), "/web");
        assert_eq!(sanitize_next(Some("/web/../admin"), "/web"), "/web");
        assert_eq!(sanitize_next(Some("web/foo"), "/web"), "/web");
    }

    #[test]
    fn sanitize_next_under_base_path() {
        assert_eq!(sanitize_next(Some("/wiki/web"), "/wiki/web"), "/wiki/web");
        assert_eq!(
            sanitize_next(Some("/wiki/web/w/a"), "/wiki/web"),
            "/wiki/web/w/a"
        );
        assert_eq!(sanitize_next(Some("/web"), "/wiki/web"), "/wiki/web");
    }

    #[test]
    fn html_auth_config_paths() {
        let cfg = HtmlAuthRedirectConfig::from_mount("", "/web");
        assert_eq!(cfg.web_root, "/web");
        assert_eq!(cfg.login_path, "/web/login");
        assert_eq!(cfg.change_password_path, "/web/change-password");

        let nested = HtmlAuthRedirectConfig::from_mount("/wiki", "/web");
        assert_eq!(nested.login_path, "/wiki/web/login");

        let root = HtmlAuthRedirectConfig::from_mount("", "/");
        assert_eq!(root.web_root, "");
        assert_eq!(root.login_path, "/login");
    }

    fn redirect_probe(status: StatusCode) -> Router {
        let cfg = Arc::new(HtmlAuthRedirectConfig::from_mount("", "/web"));
        Router::new()
            .route("/wiki", get(move || async move { status.into_response() }))
            .route(
                "/api/v1/projects",
                get(move || async move { StatusCode::UNAUTHORIZED.into_response() }),
            )
            .layer(axum::middleware::from_fn_with_state(
                cfg,
                html_auth_redirect_mw,
            ))
    }

    #[tokio::test]
    async fn html_nav_unauthorized_redirects_to_login_with_next() {
        let resp = redirect_probe(StatusCode::UNAUTHORIZED)
            .oneshot(
                Request::builder()
                    .uri("/wiki")
                    .header(header::ACCEPT, "text/html")
                    .header("sec-fetch-dest", "document")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            location.starts_with("/web/login?next="),
            "expected login redirect, got {location}"
        );
    }

    #[tokio::test]
    async fn html_nav_forbidden_redirects_to_change_password() {
        let resp = redirect_probe(StatusCode::FORBIDDEN)
            .oneshot(
                Request::builder()
                    .uri("/wiki")
                    .header(header::ACCEPT, "text/html")
                    .header("sec-fetch-dest", "document")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(location, "/web/change-password");
    }

    #[tokio::test]
    async fn api_v1_stays_json_even_for_html_navigation() {
        let resp = redirect_probe(StatusCode::UNAUTHORIZED)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .header(header::ACCEPT, "text/html")
                    .header("sec-fetch-dest", "document")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().get(header::LOCATION).is_none());
    }
}
