//! Mounting orchestration for the `/api/v1` + web-UI HTTP surfaces.
//!
//! The host binary (`ai-memory serve`) builds its MCP/hook/admin router,
//! then hands it to [`mount_web_router`] to nest the JSON API and mount
//! either the operator's custom SPA (`--web-ui-dir`) or the built-in
//! server-rendered wiki browser. Base-path / web-slug normalisation and
//! `<base href>` injection live here too so the served HTML always
//! resolves relative URLs under the configured prefix.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower::service_fn;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::info;

/// 10 MB cap on response bodies buffered for `<base href>` injection.
/// Matches the inbound body cap the HTTP server applies; a custom-SPA
/// `index.html` over the cap is misconfigured at the operator level, and
/// refusing here keeps a runaway template (or a hostile asset
/// masquerading as text/html) from streaming unbounded into memory
/// before injection.
const MAX_INJECT_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Normalise an operator-supplied path prefix into either `""` (root) or
/// `/<core>` — exactly one leading slash, no trailing slash, internal
/// empty/`//` segments collapsed.
///
/// Each segment must be a non-trivial member of the RFC 3986 *unreserved*
/// set (`ALPHA / DIGIT / "-" / "." / "_" / "~"`). Dot-segments `.` and
/// `..` are rejected outright even though their characters are unreserved
/// — at the segment level they re-encode "current directory" and "parent
/// directory" and would let a malformed env var hand the operator a
/// traversal vector through every URL the server emits.
///
/// Anything that falls outside the rule collapses to `""` (root) so a
/// bad env var can never inject markup or a protocol-relative `//` into
/// served HTML.
pub fn normalize_prefix(raw: &str) -> String {
    let segs: Vec<&str> = raw.trim().split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return String::new();
    }
    let safe = segs.iter().all(|s| {
        // Reject dot-segments (`.` / `..`) — they pass the per-char
        // unreserved test but mean "current" / "parent" at the segment
        // boundary and turn `/<base>` into `/<base>/..` traversal.
        *s != "." && *s != ".." && s.chars().all(is_unreserved_url_char)
    });
    if !safe {
        return String::new();
    }
    format!("/{}", segs.join("/"))
}

/// RFC 3986 `unreserved = ALPHA / DIGIT / "-" / "." / "_" / "~"`. Kept
/// here as a single source of truth for both the path-prefix charset
/// check and any future per-segment validation.
fn is_unreserved_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~')
}

/// Build the `<base href>` value (always trailing-slash-terminated, never
/// the protocol-relative `//`) for the web UI mounted at `base_path` +
/// `web_slug`.
pub fn web_base_href(base_path: &str, web_slug: &str) -> String {
    let combined = format!(
        "{}{}",
        normalize_prefix(base_path),
        normalize_prefix(web_slug)
    );
    if combined.is_empty() {
        "/".to_string()
    } else {
        format!("{combined}/")
    }
}

/// Escape a string for safe inclusion inside a double-quoted HTML attribute.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Insert `snippet` immediately after the first `<head…>` tag (or prepend
/// it when there is no head).
///
/// Matches outside HTML comments only: `<!-- … <head … --> <head>` skips
/// the comment-internal occurrence and injects after the real `<head>`.
/// Anything inside `<textarea>`, `<script>`, or other raw-text elements
/// is NOT specially handled — built-in askama templates never put
/// `<head` in those, and a custom `--web-ui-dir` SPA that does is a
/// misconfiguration the operator can fix at the source. Avoiding a
/// full HTML parser here keeps injection a single pass + alloc.
fn inject_into_head(html: &str, snippet: &str) -> String {
    let bytes = html.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Skip past any HTML comment opening here so a `<head` literal
        // sitting inside it cannot win the search.
        if html[cursor..].starts_with("<!--") {
            match html[cursor..].find("-->") {
                Some(end) => cursor += end + 3,
                None => break, // Unterminated comment — bail out of injection.
            }
            continue;
        }
        if html[cursor..].starts_with("<head")
            && let Some(gt) = html[cursor..].find('>')
        {
            let pos = cursor + gt + 1;
            let mut out = String::with_capacity(html.len() + snippet.len());
            out.push_str(&html[..pos]);
            out.push_str(snippet);
            out.push_str(&html[pos..]);
            return out;
        }
        cursor += 1;
    }
    format!("{snippet}{html}")
}

/// Inject `<base href="{href}">` so the served SPA's relative asset/router
/// URLs resolve under the configured prefix.
pub fn inject_base_href(html: &str, href: &str) -> String {
    inject_into_head(html, &format!("<base href=\"{}\">", escape_attr(href)))
}

/// Inject `<meta name="ai-memory-base-path" content="{base_path}">` so the
/// SPA can build API URLs as `{base_path}/api/v1`. The `<base href>` alone is
/// ambiguous for this because it also folds in the web slug (e.g. `/web`),
/// whereas `/api/v1` hangs off the base path, not the web mount.
pub fn inject_base_path_meta(html: &str, base_path: &str) -> String {
    inject_into_head(
        html,
        &format!(
            "<meta name=\"ai-memory-base-path\" content=\"{}\">",
            escape_attr(base_path)
        ),
    )
}

#[cfg(test)]
mod web_base_tests {
    use super::{inject_base_href, inject_base_path_meta, normalize_prefix, web_base_href};

    #[test]
    fn normalize_prefix_edge_cases() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("//"), "");
        assert_eq!(normalize_prefix("  /  "), "");
        assert_eq!(normalize_prefix("wiki"), "/wiki");
        assert_eq!(normalize_prefix("/wiki"), "/wiki");
        assert_eq!(normalize_prefix("/wiki/"), "/wiki");
        assert_eq!(normalize_prefix("//wiki//"), "/wiki");
        assert_eq!(normalize_prefix("/wiki/sub"), "/wiki/sub");
        // Unsafe chars fall back to root — never inject markup or `//`.
        assert_eq!(normalize_prefix("/wi\"ki"), "");
        assert_eq!(normalize_prefix("/wiki space"), "");
        assert_eq!(normalize_prefix("/<script>"), "");
    }

    /// Dot-segments must NOT survive normalisation. Their characters
    /// pass the unreserved per-char allowlist (`.` is unreserved), so
    /// the segment-level rejection is what stops `/..` and `/.` from
    /// turning the base prefix into a traversal vector. Regression
    /// guard — without this, `AI_MEMORY_BASE_PATH=/..` would serve
    /// `/..` and let an upstream redirect normalise it to `/`.
    #[test]
    fn normalize_prefix_rejects_dot_segments() {
        assert_eq!(normalize_prefix("/.."), "", "/.. must collapse to root");
        assert_eq!(normalize_prefix("/."), "", "/. must collapse to root");
        assert_eq!(
            normalize_prefix("/wiki/.."),
            "",
            "any embedded /.. fails the whole prefix"
        );
        assert_eq!(
            normalize_prefix("/wiki/./sub"),
            "",
            "any embedded /. fails the whole prefix"
        );
        // Segments that merely START with a dot but aren't pure
        // dot-segments are still valid (RFC 3986 unreserved chars).
        assert_eq!(normalize_prefix("/.wellknown"), "/.wellknown");
    }

    /// Nested base paths (`/a/b/c`) are valid; the normaliser keeps the
    /// hierarchy intact instead of collapsing to one level.
    #[test]
    fn normalize_prefix_keeps_nested_paths() {
        assert_eq!(normalize_prefix("/a/b/c"), "/a/b/c");
        assert_eq!(normalize_prefix("a/b/c"), "/a/b/c");
        assert_eq!(normalize_prefix("//a//b//c//"), "/a/b/c");
        assert_eq!(normalize_prefix("/a/b/c/d/e"), "/a/b/c/d/e");
    }

    /// `inject_into_head` must skip `<head` literals sitting inside an
    /// HTML comment, otherwise a custom SPA whose `index.html` had a
    /// `<!-- <head> placeholder -->` comment would have the snippet
    /// injected at the wrong place.
    #[test]
    fn inject_base_href_skips_head_inside_html_comment() {
        let html =
            "<!-- <head fake --><html><head><meta charset=\"utf-8\"></head><body></body></html>";
        let out = inject_base_href(html, "/w/");
        // Snippet must follow the REAL <head>, not the commented one —
        // the comment must remain unmodified.
        assert!(out.contains("<!-- <head fake -->"));
        assert!(out.contains("<head><base href=\"/w/\"><meta"));
    }

    /// `inject_into_head` falls back to the prepend path on an
    /// unterminated comment instead of looping forever (defensive).
    #[test]
    fn inject_base_href_on_unterminated_comment_falls_back_to_prepend() {
        let html = "<!-- never closes <head>";
        let out = inject_base_href(html, "/w/");
        assert!(out.starts_with("<base href=\"/w/\">"));
    }

    #[test]
    fn web_base_href_never_protocol_relative() {
        assert_eq!(web_base_href("", "/web"), "/web/");
        assert_eq!(web_base_href("/wiki", "/web"), "/wiki/web/");
        assert_eq!(web_base_href("/wiki", "/"), "/wiki/");
        assert_eq!(web_base_href("", "/"), "/");
        assert_eq!(web_base_href("/", "/"), "/");
        assert_eq!(web_base_href("/wiki/", "web"), "/wiki/web/");
        for (b, s) in [("", "/"), ("/", "/"), ("//", "//")] {
            assert!(!web_base_href(b, s).starts_with("//"));
        }
    }

    #[test]
    fn inject_base_href_after_head() {
        let html = "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";
        let out = inject_base_href(html, "/wiki/web/");
        assert!(out.contains("<head><base href=\"/wiki/web/\"><meta"));
    }

    #[test]
    fn inject_base_href_no_head_prepends() {
        let out = inject_base_href("<html></html>", "/x/");
        assert!(out.starts_with("<base href=\"/x/\"><html>"));
    }

    #[test]
    fn inject_base_href_escapes_attr() {
        let out = inject_base_href("<head></head>", "/a\"b/");
        assert!(out.contains("<base href=\"/a&quot;b/\">"));
    }

    #[test]
    fn inject_base_path_meta_emits_meta() {
        let out = inject_base_path_meta("<head></head>", "/wiki");
        assert!(out.contains("<meta name=\"ai-memory-base-path\" content=\"/wiki\">"));
        // Empty base path => empty content (SPA falls back to root).
        let empty = inject_base_path_meta("<head></head>", "");
        assert!(empty.contains("content=\"\""));
    }
}

/// Path / URL config the web mount needs. Bundling these together
/// keeps `mount_web_router` and its helpers under clippy's
/// `too_many_arguments` threshold without `#[allow]` papering over
/// the call shape.
pub struct WebMountSpec<'a> {
    /// Operator-supplied custom SPA directory (`--web-ui-dir`). `None`
    /// mounts the built-in server-rendered wiki browser instead.
    pub web_ui_dir: Option<&'a Path>,
    /// Pre-validated CORS origins scoped to `/api/v1` only.
    pub cors_origins: &'a [String],
    /// Raw `--web-slug` value; normalised internally by the mount.
    pub web_slug: &'a str,
    /// `<base href>` value (base path + slug, trailing slash) injected
    /// into every served HTML page.
    pub base_href: &'a str,
    /// Normalised base path (`""` or `/<core>`) the whole surface is
    /// nested under; stamped into the `ai-memory-base-path` meta tag.
    pub base_path: &'a str,
}

/// Orchestrator: assemble the `/api/v1` + web-UI surfaces on top of
/// `router`. Skips everything when web is disabled. Each concern lives
/// in a dedicated helper below so this function reads as a four-step
/// recipe (CORS-scoped API, slug normalisation, SPA-vs-builtin choice,
/// final mount).
pub fn mount_web_router(
    router: axum::Router,
    enable_web: bool,
    reader: ReaderPool,
    wiki: Wiki,
    spec: WebMountSpec<'_>,
) -> Result<axum::Router> {
    if !enable_web {
        return Ok(router);
    }
    // Register the web surfaces BEFORE applying the bearer middleware. In
    // axum 0.8, `.layer()` only attaches to routes registered before the
    // call; nesting after the layer would silently bypass auth for /web/*.
    let router = router.nest(
        "/api/v1",
        build_api_router(&reader, &wiki, spec.cors_origins),
    );

    // Where the UI is mounted WITHIN the (already-applied) base path.
    // Empty slug => the UI is the root of the base path itself.
    let slug = normalize_prefix(spec.web_slug);
    let mount = if slug.is_empty() { "/" } else { slug.as_str() };

    // Custom SPA via --web-ui-dir (SPA fallback to index.html), otherwise
    // the built-in server-side wiki browser. In both cases the served
    // index carries an injected `<base href>` so relative asset/router
    // URLs resolve under `{base_path}{web_slug}`.
    if let Some(dir) = spec.web_ui_dir {
        return mount_custom_spa(router, dir, &slug, spec.base_href, spec.base_path, mount);
    }
    Ok(mount_builtin_browser(
        router,
        reader,
        wiki,
        &slug,
        spec.base_href,
        mount,
    ))
}

/// Build the `/api/v1` router and apply the per-origin CORS layer if
/// the operator configured any. The layer is scoped to this router only
/// (CORS_NOT_APPLIED_TO_OTHER_ROUTES invariant — `/mcp`, `/hook`,
/// `/admin`, and `/web` must remain CORS-free).
fn build_api_router(reader: &ReaderPool, wiki: &Wiki, cors_origins: &[String]) -> axum::Router {
    let api = crate::api_router(reader.clone(), wiki.clone());
    if cors_origins.is_empty() {
        return api;
    }
    // Origins were already validated before binding, so parsing here
    // is expected to succeed; `.expect` surfaces a logic bug if it does not.
    let parsed: Vec<axum::http::HeaderValue> = cors_origins
        .iter()
        .map(|o| o.parse().expect("pre-validated origin must parse"))
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(parsed)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .allow_credentials(true)
        .max_age(Duration::from_secs(600));
    info!(origins = ?cors_origins, "CORS layer attached to /api/v1");
    api.layer(cors)
}

/// Mount the operator's custom SPA from `--web-ui-dir`. Reads
/// `index.html`, injects `<base href>` plus the `ai-memory-base-path`
/// meta, and serves the rest as static assets with an SPA fallback to
/// the injected shell. Errors surface as `anyhow` with the dir path.
fn mount_custom_spa(
    router: axum::Router,
    dir: &Path,
    slug: &str,
    base_href: &str,
    base_path: &str,
    mount: &str,
) -> Result<axum::Router> {
    let dir = dir.to_path_buf();
    let raw = std::fs::read_to_string(dir.join("index.html"))
        .with_context(|| format!("reading custom web UI index at {}", dir.display()))?;
    let injected = inject_base_path_meta(&inject_base_href(&raw, base_href), base_path);
    info!(mount, base_href, base_path, "custom web UI mounted");
    let spa = custom_spa_router(dir, injected.clone());
    Ok(if slug.is_empty() {
        router.merge(spa)
    } else {
        // `nest(slug, …)` routes `{slug}` (→ inner `/`) and `{slug}/<path>`
        // (→ inner `/{*path}`), but NOT the bare trailing-slash root
        // `{slug}/` — that empty sub-path matches neither, so it 404s. The
        // SPA router normalises its home to exactly that URL, so a refresh on
        // the app root returned a hard 404 (`custom_spa_trailing_slash_root_*`).
        // Serve the injected shell there too. Unlike the builtin browser —
        // which redirects `{slug}/` → `{slug}` — a SPA is happier staying put
        // on a 200 than bouncing through a redirect on every root refresh.
        let slash_index = Arc::new(injected);
        router
            .route(
                &format!("{slug}/"),
                axum::routing::get(move || {
                    let body = slash_index.clone();
                    async move { axum::response::Html((*body).clone()) }
                }),
            )
            .nest(slug, spa)
    })
}

/// Mount the built-in server-rendered wiki browser at `slug` with
/// `<base href>` injection middleware. When `slug` is non-empty also
/// register the trailing-slash → canonical redirect, preserving any
/// query string the caller passed.
fn mount_builtin_browser(
    router: axum::Router,
    reader: ReaderPool,
    wiki: Wiki,
    slug: &str,
    base_href: &str,
    mount: &str,
) -> axum::Router {
    // The built-in browser emits RELATIVE asset/link URLs (`static/…`,
    // `w/…`, `search`, `.`). Inject a `<base href>` into every HTML
    // response so they resolve under `{base_path}{web_slug}/` — the
    // same anchoring the custom SPA gets via its injected index.
    let web_router = crate::router(reader, wiki).layer(axum::middleware::from_fn_with_state(
        Arc::new(base_href.to_string()),
        inject_web_base_href,
    ));
    info!(mount, base_href, "read-only wiki browser mounted");
    if slug.is_empty() {
        return router.merge(web_router);
    }
    // Strip-trailing-slash redirect. The target must carry the full
    // external prefix because the surrounding `nest(&base_path, …)`
    // does NOT rewrite Location headers. Derive it from base_href
    // (which already folds in base_path + slug). The closure takes
    // `Uri` so `?q=x` survives — see
    // `trailing_slash_redirect_preserves_query_string`.
    let canonical = {
        let trimmed = base_href.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    };
    router
        .route(
            &format!("{slug}/"),
            axum::routing::get(move |uri: axum::http::Uri| {
                let to = match uri.query() {
                    Some(q) if !q.is_empty() => format!("{canonical}?{q}"),
                    _ => canonical.clone(),
                };
                async move { axum::response::Redirect::permanent(&to) }
            }),
        )
        .nest(slug, web_router)
}

fn custom_spa_router(dir: std::path::PathBuf, injected_index: String) -> axum::Router {
    let index = Arc::new(injected_index);
    let root_index = index.clone();
    let direct_index = index.clone();
    let fallback_index = index.clone();

    // Assets are served as files; any missing asset path falls back to the
    // injected index for SPA client routes. Direct `/index.html` is routed
    // explicitly so it cannot bypass injection by being served from disk.
    let assets = ServeDir::new(dir)
        .append_index_html_on_directories(false)
        .fallback(service_fn(move |_req: Request<Body>| {
            let body = fallback_index.clone();
            async move {
                Ok::<_, Infallible>(axum::response::Html((*body).clone()).into_response())
            }
        }));

    axum::Router::new()
        .route(
            "/",
            axum::routing::get(move || {
                let body = root_index.clone();
                async move { axum::response::Html((*body).clone()) }
            }),
        )
        .route(
            "/index.html",
            axum::routing::get(move || {
                let body = direct_index.clone();
                async move { axum::response::Html((*body).clone()) }
            }),
        )
        .route_service("/{*path}", assets)
}

/// Response middleware: inject `<base href>` into `text/html` responses from
/// the built-in server-rendered web browser, so its relative URLs resolve
/// under the configured `{base_path}{web_slug}` prefix. Non-HTML responses
/// (static assets, redirects) pass through untouched.
async fn inject_web_base_href(
    State(base_href): State<Arc<String>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let resp = next.run(req).await;
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if !is_html {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    // Bound the buffer at MAX_INJECT_BODY_BYTES — same cap inbound bodies use.
    // A custom-SPA `index.html` over the cap is misconfigured at the
    // operator level; refusing here keeps a runaway template (or a
    // hostile asset masquerading as text/html) from streaming
    // unbounded into memory before injection.
    let bytes = match axum::body::to_bytes(body, MAX_INJECT_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "response body too large or read error\n",
            )
                .into_response();
        }
    };
    match std::str::from_utf8(&bytes) {
        Ok(html) => {
            let injected = inject_base_href(html, &base_href);
            // Stale length from the pre-injection body; let hyper recompute.
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(injected))
        }
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_store::Store;
    use tempfile::TempDir;
    use tower::ServiceExt;
    /// Assemble the web/API surface under `base_path` + `web_slug` exactly the
    /// way the `serve` handler does (mount, then `nest(&base_path, …)`), with
    /// no auth/host layers so tests probe routing + injection in isolation.
    /// Returns the `TempDir` guard too — the caller must keep it alive for the
    /// router's lifetime (the store's SQLite + wiki files live under it).
    fn based_web_router(base_path: &str, web_slug: &str) -> (TempDir, axum::Router) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let base = normalize_prefix(base_path);
        let base_href = web_base_href(base_path, web_slug);
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &[],
                web_slug,
                base_href: &base_href,
                base_path: &base,
            },
        )
        .unwrap();
        let router = if base.is_empty() {
            router
        } else {
            axum::Router::new().nest(&base, router)
        };
        // Mirror the production favicon mount in serve handler: at the
        // absolute host root, outside the base-path nest.
        let router = router.merge(crate::favicon_router());
        (tmp, router)
    }

    #[tokio::test]
    async fn base_path_nests_all_surfaces_and_root_404s() {
        let (_tmp, router) = based_web_router("/wiki", "/web");

        // The web UI is reachable UNDER the prefix…
        let under = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/web")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(under.status(), StatusCode::OK);

        // …and the API too.
        let api = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::OK);

        // The same paths at the host ROOT must 404 — nothing leaks outside the
        // prefix (the whole point of base-path hosting behind a shared proxy).
        for uri in ["/web", "/api/v1/projects"] {
            let root = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                root.status(),
                StatusCode::NOT_FOUND,
                "{uri} must 404 at root"
            );
        }
    }

    #[tokio::test]
    async fn inject_web_base_href_targets_html_only() {
        let (_tmp, router) = based_web_router("/wiki", "/web");

        // HTML response carries the injected <base href> under the prefix.
        let html_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/web")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(html_resp.status(), StatusCode::OK);
        let html = axum::body::to_bytes(html_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = std::str::from_utf8(&html).unwrap();
        assert!(
            html.contains(r#"<base href="/wiki/web/">"#),
            "expected injected base href, got: {html}"
        );

        // A non-HTML asset passes through untouched (no <base> smuggled in).
        let css_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/static/tailwind.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(css_resp.status(), StatusCode::OK);
        let css = axum::body::to_bytes(css_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            !std::str::from_utf8(&css).unwrap().contains("<base href"),
            "non-HTML asset must not receive a <base href> injection"
        );
    }

    #[tokio::test]
    async fn trailing_slash_redirect_carries_the_prefix() {
        let (_tmp, router) = based_web_router("/wiki", "/web");
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        // Location must include the external prefix — the surrounding base nest
        // does NOT rewrite Location headers, so a bare `/web` would drop `/wiki`.
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/wiki/web",);
    }

    /// Trailing-slash redirect must preserve the query string. The
    /// original handler took `()` and silently dropped `?q=x`, so a
    /// link in the SPA that appended a filter param round-tripped to
    /// the canonical URL with the param missing. Fragments are
    /// client-only and never reach the server, so we only assert query.
    #[tokio::test]
    async fn trailing_slash_redirect_preserves_query_string() {
        let (_tmp, router) = based_web_router("/wiki", "/web");
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/?q=foo&limit=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/wiki/web?q=foo&limit=5",
            "redirect must carry the original query, not drop it"
        );
    }

    /// Nested base paths (`/a/b/c`) — exercised by the normaliser
    /// unit test but not previously end-to-end. Routes must be
    /// reachable under the full prefix and 404 at any shorter prefix.
    #[tokio::test]
    async fn nested_base_path_nests_web_and_api() {
        let (_tmp, router) = based_web_router("/a/b/c", "/web");
        // Full nested prefix reaches both surfaces.
        for uri in ["/a/b/c/web", "/a/b/c/api/v1/projects"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{uri} must reach the surface"
            );
        }
        // A SHORTER prefix (e.g. only /a/b) must NOT leak — the nest is
        // exactly `/a/b/c` and any partial mount is unmapped.
        for uri in ["/a/b/web", "/a/api/v1/projects"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{uri} must 404 — leaks the prefix otherwise"
            );
        }
    }

    /// Custom-SPA index that already carries its own `<base href>` —
    /// today we prepend another one; HTML5 says the browser honours
    /// the FIRST `<base>` so the injection becomes a silent no-op.
    /// Documented behaviour (see `inject_into_head` doc-comment) but
    /// also exercised here so a future change to "replace existing"
    /// gets noticed by a failing test rather than silently shipping.
    #[test]
    fn inject_base_href_with_existing_base_tag_does_not_replace() {
        let html = "<html><head><base href=\"/old/\"><title>x</title></head></html>";
        let out = inject_base_href(html, "/new/");
        // Injected snippet appears first, the old <base> remains too.
        let new_pos = out.find("<base href=\"/new/\">").expect("new injected");
        let old_pos = out.find("<base href=\"/old/\">").expect("old preserved");
        assert!(
            new_pos < old_pos,
            "injected base must appear before the pre-existing one (browser ignores duplicates after the first)"
        );
    }

    /// Post-merge audit (Phase 7 live test) caught that PR #79's
    /// `/favicon.ico` route was nested inside `/web`, so it lived at
    /// `/web/favicon.ico` and the browser's automatic root fetch always
    /// 404'd. Fix: a separate `favicon_router()` mounted at the absolute
    /// HOST root, outside `--base-path` and outside the `/web` nest.
    /// This test pins both:
    ///   * `/favicon.ico` at the host root returns the PNG.
    ///   * Under `--base-path /wiki`, the favicon STAYS at root —
    ///     browsers fetch `<host>/favicon.ico` regardless of where the
    ///     app is mounted; the route must not move with the prefix.
    #[tokio::test]
    async fn favicon_lives_at_host_root_regardless_of_base_path() {
        for base_path in ["", "/wiki"] {
            let (_tmp, router) = based_web_router(base_path, "/web");
            let resp = router
                .oneshot(
                    Request::builder()
                        .uri("/favicon.ico")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "/favicon.ico must be reachable at host root (base_path={base_path:?})"
            );
            assert_eq!(
                resp.headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("image/png"),
            );
        }
    }

    #[tokio::test]
    async fn no_base_path_is_byte_equivalent_at_root() {
        let (_tmp, router) = based_web_router("", "/web");
        let resp = router
            .clone()
            .oneshot(Request::builder().uri("/web").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(r#"<base href="/web/">"#),
            "default mount should inject the root-relative base href"
        );
    }

    #[tokio::test]
    async fn custom_spa_index_routes_are_injected_under_base_path() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<!doctype html><html><head><title>spa</title></head><body>shell</body></html>",
        )
        .unwrap();
        std::fs::write(ui.path().join("app.js"), "console.log('asset');").unwrap();

        let base = normalize_prefix("/wiki");
        let base_href = web_base_href("/wiki", "/web");
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/web",
                base_href: &base_href,
                base_path: &base,
            },
        )
        .unwrap();
        let router = axum::Router::new().nest(&base, router);

        for uri in [
            "/wiki/web",
            "/wiki/web/", // trailing-slash root: SPA home after router normalises it
            "/wiki/web/index.html",
            "/wiki/web/client/route",
        ] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(
                html.contains(r#"<base href="/wiki/web/">"#),
                "{uri} must receive injected base href: {html}"
            );
            assert!(
                html.contains(r#"<meta name="ai-memory-base-path" content="/wiki">"#),
                "{uri} must receive injected API base-path meta: {html}"
            );
            assert!(html.contains("shell"), "{uri} returns the SPA shell");
        }

        let asset = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        let body = axum::body::to_bytes(asset.into_body(), usize::MAX)
            .await
            .unwrap();
        let js = std::str::from_utf8(&body).unwrap();
        assert_eq!(js, "console.log('asset');");
    }

    /// Regression for the prod 404: with `base_path=""` (host root) the custom
    /// SPA mounts at slug `/web`. `nest("/web", …)` served `/web` and
    /// `/web/<route>` but left the bare trailing-slash root `/web/` unrouted →
    /// hard 404. The SPA normalises its home to exactly `/web/`, so refreshing
    /// the app root broke (both a host-root deploy and one mounted under a base
    /// path like `/wiki`).
    #[tokio::test]
    async fn custom_spa_trailing_slash_root_serves_shell() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<!doctype html><html><head><title>spa</title></head><body>shell</body></html>",
        )
        .unwrap();

        let base_href = web_base_href("", "/web");
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/web",
                base_href: &base_href,
                base_path: "",
            },
        )
        .unwrap();

        // `/web`, the trailing-slash root `/web/`, and a deep client route all
        // serve the injected shell — none may 404.
        for uri in ["/web", "/web/", "/web/projects/x"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{uri} must serve the SPA shell, not 404"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(
                html.contains("shell"),
                "{uri} returns the SPA shell: {html}"
            );
            assert!(
                html.contains(r#"<base href="/web/">"#),
                "{uri} must receive the injected base href: {html}"
            );
        }
    }

    #[tokio::test]
    async fn custom_spa_root_slug_does_not_shadow_api_routes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<!doctype html><html><head><title>spa</title></head><body>root shell</body></html>",
        )
        .unwrap();
        std::fs::write(ui.path().join("app.js"), "console.log('root asset');").unwrap();

        let base = normalize_prefix("/wiki");
        let base_href = web_base_href("/wiki", "/");
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/",
                base_href: &base_href,
                base_path: &base,
            },
        )
        .unwrap();
        let router = axum::Router::new().nest(&base, router);

        for uri in ["/wiki", "/wiki/index.html", "/wiki/client/route"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(
                html.contains(r#"<base href="/wiki/">"#),
                "{uri} must receive injected base href: {html}"
            );
            assert!(
                html.contains(r#"<meta name="ai-memory-base-path" content="/wiki">"#),
                "{uri} must receive injected API base-path meta: {html}"
            );
            assert!(html.contains("root shell"), "{uri} returns the SPA shell");
        }

        let api = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::OK);
        let content_type = api
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("application/json"),
            "API route must not be shadowed by the root SPA: {content_type}"
        );

        let asset = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        let body = axum::body::to_bytes(asset.into_body(), usize::MAX)
            .await
            .unwrap();
        let js = std::str::from_utf8(&body).unwrap();
        assert_eq!(js, "console.log('root asset');");
    }
    #[tokio::test]
    async fn cors_layer_on_api_v1_allows_configured_origin() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();

        let cors_origins = ["https://app.example.com".to_string()];
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &cors_origins,
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        // No auth layer so we can reach /api/v1 directly.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/workspaces")
                    .header("Origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let acao = resp
            .headers()
            .get("access-control-allow-origin")
            .expect("ACAO header must be present for allowed origin")
            .to_str()
            .unwrap();
        assert_eq!(acao, "https://app.example.com");
        let acac = resp
            .headers()
            .get("access-control-allow-credentials")
            .expect("ACAC header must be present")
            .to_str()
            .unwrap();
        assert_eq!(acac, "true");
    }

    #[tokio::test]
    async fn cors_layer_on_api_v1_denies_unlisted_origin() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();

        let cors_origins = ["https://app.example.com".to_string()];
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &cors_origins,
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/workspaces")
                    .header("Origin", "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // The request is still served (CORS does not block on the server side),
        // but the ACAO header must be absent so the browser enforces the policy.
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "unlisted origin must not receive ACAO header"
        );
    }

    #[tokio::test]
    async fn cors_not_applied_to_other_routes() {
        // /mcp and /admin routes must not carry CORS headers even when
        // a CORS origin list is configured (CORS_NOT_APPLIED_TO_OTHER_ROUTES
        // invariant). We verify by checking that a request to a non-/api/v1
        // path that 404s (no actual handler mounted here) still lacks ACAO.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let cors_origins = ["https://app.example.com".to_string()];
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &cors_origins,
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        // /web is a non-api route; sending an Origin header must not trigger CORS.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "/web must not carry CORS headers: {:?}",
            resp.headers()
        );
    }
}
