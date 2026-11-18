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

/// The Memory Archive faces (Latin subsets of `@fontsource` 5.3.0, OFL; see
/// `static/fonts/font-licenses.txt`), by the file name the stylesheet's
/// `@font-face` rules request. One table for `/web` and the management
/// console; nothing outside it is served.
pub(crate) fn font_file(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "ibm-plex-sans-latin-400-normal.woff2" => {
            include_bytes!("../../static/fonts/ibm-plex-sans-latin-400-normal.woff2")
        }
        "ibm-plex-sans-latin-500-normal.woff2" => {
            include_bytes!("../../static/fonts/ibm-plex-sans-latin-500-normal.woff2")
        }
        "ibm-plex-sans-latin-600-normal.woff2" => {
            include_bytes!("../../static/fonts/ibm-plex-sans-latin-600-normal.woff2")
        }
        "chakra-petch-latin-500-normal.woff2" => {
            include_bytes!("../../static/fonts/chakra-petch-latin-500-normal.woff2")
        }
        "chakra-petch-latin-600-normal.woff2" => {
            include_bytes!("../../static/fonts/chakra-petch-latin-600-normal.woff2")
        }
        "jetbrains-mono-latin-400-normal.woff2" => {
            include_bytes!("../../static/fonts/jetbrains-mono-latin-400-normal.woff2")
        }
        "jetbrains-mono-latin-500-normal.woff2" => {
            include_bytes!("../../static/fonts/jetbrains-mono-latin-500-normal.woff2")
        }
        _ => return None,
    })
}

/// `GET /static/{font}`: self-hosted fonts shared with the management surface.
pub(crate) async fn font(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response {
    match font_file(&name) {
        Some(bytes) => ([(header::CONTENT_TYPE, "font/woff2")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
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
    /// #2003: the shipped faces are Latin subsets, so the Unicode characters
    /// the templates still show (arrows, separators) fall through every text
    /// stack. Each stack has to reach a symbol face, after `system-ui` so
    /// Latin text keeps the platform font it had. Rules read the stacks
    /// through `--ma-font-sans` and `--ma-font-display`.
    #[test]
    fn text_font_stacks_reach_a_symbol_face_after_system_ui() {
        let stacks: Vec<&str> = ["--ma-font-sans:", "--ma-font-display:"]
            .iter()
            .filter_map(|property| {
                super::TAILWIND_CSS
                    .split(property)
                    .nth(1)
                    .and_then(|rest| rest.split([';', '}']).next())
            })
            .collect();
        assert_eq!(stacks.len(), 2, "a text font stack is missing");
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

    /// Each `@font-face` the stylesheet declares resolves to an embedded
    /// file, so renaming a face in one place without the other fails here
    /// rather than as a silent fallback font in the browser.
    #[test]
    fn every_stylesheet_font_is_embedded() {
        let names: Vec<&str> = super::TAILWIND_CSS
            .split("url(")
            .skip(1)
            .filter_map(|rest| rest.split(')').next())
            .map(|url| url.trim_matches(['"', '\'']))
            .filter(|url| url.ends_with(".woff2"))
            .collect();
        assert_eq!(names.len(), 7, "{names:?}");
        for name in names {
            assert!(super::font_file(name).is_some(), "{name} is not embedded");
        }
        assert!(super::font_file("../config.toml").is_none());
    }
}
