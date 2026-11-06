//! Static asset handlers: Tailwind CSS and logo image.
//!
//! Assets are embedded at compile time so the binary is fully
//! self-contained — no runtime file access needed.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;

/// Compiled Tailwind CSS. The path is emitted by `build.rs` via the
/// `AI_MEMORY_WEB_TAILWIND_CSS` cargo env var.
static TAILWIND_CSS: &str = include_str!(env!("AI_MEMORY_WEB_TAILWIND_CSS"));

/// Logo (PNG), embedded at compile time from `docs/logo.png`.
static LOGO: &[u8] = include_bytes!("../../../../docs/logo.png");

/// Self-hosted fonts shared with the management surface.
pub(crate) async fn font(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response {
    let bytes: &[u8] = match name.as_str() {
        "instrument-sans-latin-wdth-normal.woff2" => {
            include_bytes!("../../static/management/instrument-sans-latin-wdth-normal.woff2")
        }
        "bricolage-grotesque-latin-standard-normal.woff2" => include_bytes!(
            "../../static/management/bricolage-grotesque-latin-standard-normal.woff2"
        ),
        "commit-mono-latin-400-normal.woff2" => {
            include_bytes!("../../static/management/commit-mono-latin-400-normal.woff2")
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, "font/woff2")], bytes).into_response()
}

/// `GET /static/tailwind.css`
pub(crate) async fn tailwind_css() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    (StatusCode::OK, headers, TAILWIND_CSS)
}

/// `GET /static/logo.png`
pub(crate) async fn logo() -> impl IntoResponse {
    png_response(LOGO)
}

/// `GET /favicon.ico` — same PNG as the header logo (browsers request this path by default).
pub(crate) async fn favicon() -> impl IntoResponse {
    png_response(LOGO)
}

fn png_response(bytes: &'static [u8]) -> (StatusCode, HeaderMap, &'static [u8]) {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    (StatusCode::OK, headers, bytes)
}

#[cfg(test)]
mod tests {
    /// #2003: the three shipped faces are Latin subsets, so the Unicode
    /// characters the templates use as icons fall through every text stack.
    /// Each stack has to reach a symbol face, after `system-ui` so Latin text
    /// keeps the platform font it had.
    #[test]
    fn text_font_stacks_reach_a_symbol_face_after_system_ui() {
        let stacks: Vec<&str> = super::TAILWIND_CSS
            .split("font-family:")
            .skip(1)
            .filter_map(|rest| rest.split([';', '}']).next())
            .filter(|stack| {
                stack.starts_with("Management Sans,") || stack.starts_with("Management Display,")
            })
            .collect();
        assert!(!stacks.is_empty(), "no text font stack in the stylesheet");
        for stack in stacks {
            let system_ui = stack.find("system-ui").unwrap_or_else(|| panic!("{stack}"));
            for face in [
                "Apple Symbols",
                "Segoe UI Symbol",
                "Noto Sans Symbols",
                "DejaVu Sans",
            ] {
                let at = stack
                    .find(face)
                    .unwrap_or_else(|| panic!("{face} missing from {stack}"));
                assert!(system_ui < at, "{face} precedes system-ui in {stack}");
            }
        }
    }
}
