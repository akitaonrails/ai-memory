//! Authenticated, build-matched help with caller-visible installation states.

use crate::{
    documentation::{self, Document},
    markdown,
    templates::VERSION,
};
use crate::{features::FeatureState, state::WebState};
use ai_memory_core::AuthLevel;
use askama::Template;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Default, Deserialize)]
pub(crate) struct DocsQuery {
    #[serde(default)]
    q: String,
    release: Option<String>,
}

#[derive(Template)]
#[template(path = "documentation_index.html")]
struct IndexView {
    pub i18n: ai_memory_core::i18n::Localizer,
    query: String,
    docs: Vec<&'static Document>,
}

struct ReleaseOption {
    id: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "documentation_page.html")]
struct DocumentView {
    pub i18n: ai_memory_core::i18n::Localizer,
    doc: &'static Document,
    html: String,
    outline: Vec<markdown::OutlineEntry>,
    current_href: String,
    markdown_href: String,
    releases: Vec<ReleaseOption>,
    selected_release: String,
    sidebar: &'static [Document],
    features: Vec<FeatureState>,
}

fn response(body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    response
}

pub(crate) async fn index(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    Query(query): Query<DocsQuery>,
) -> Response {
    if query.q.chars().count() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let docs = documentation::search(&query.q);
    match (IndexView {
        i18n: i18n.clone(),
        query: query.q,
        docs,
    })
    .render()
    {
        Ok(html) => response(Html(html)),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn selected<'a>(
    doc: &'a Document,
    requested: Option<&str>,
) -> Result<(&'a str, String, Vec<ReleaseOption>), StatusCode> {
    let releases = if doc.id.starts_with("changelog") {
        documentation::releases(doc.body)
    } else {
        Vec::new()
    };
    if releases.is_empty() {
        return if requested.is_some() {
            Err(StatusCode::BAD_REQUEST)
        } else {
            Ok((doc.body, String::new(), Vec::new()))
        };
    }
    let id = requested.unwrap_or(releases[0].id);
    let body = if id == "all" {
        doc.body
    } else {
        releases
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.body)
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let mut options: Vec<_> = releases
        .iter()
        .map(|r| ReleaseOption {
            id: r.id.into(),
            label: r.heading.trim_start_matches("## ").into(),
            selected: r.id == id,
        })
        .collect();
    options.push(ReleaseOption {
        id: "all".into(),
        label: "All entries in this series".into(),
        selected: id == "all",
    });
    Ok((body, id.into(), options))
}

pub(crate) async fn page(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    auth: Option<axum::Extension<AuthLevel>>,
    Path(slug): Path<String>,
    Query(query): Query<DocsQuery>,
) -> Response {
    let raw = slug.ends_with(".md");
    let Some(doc) = documentation::find(slug.strip_suffix(".md").unwrap_or(&slug)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (body, selected_release, releases) = match selected(doc, query.release.as_deref()) {
        Ok(s) => s,
        Err(s) => return s.into_response(),
    };
    if raw {
        return response((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            body,
        ));
    }
    let suffix = if selected_release.is_empty() {
        String::new()
    } else {
        format!(
            "?release={}",
            crate::templates::encode_segment(&selected_release)
        )
    };
    let current_href = format!("docs/{}{suffix}", doc.id);
    let rendered = markdown::render_reference_document(body, |url| {
        documentation::link(doc, &current_href, url)
    });
    let features = match crate::features::states(&state, auth.map(|v| v.0)).await {
        Ok(rows) => rows,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let features = features
        .into_iter()
        .filter(|f| {
            doc.id == "features"
                || (doc.id != "changelog"
                    && !doc.id.starts_with("changelog")
                    && ["install", "usage", "cookbook", "messaging", "architecture"]
                        .contains(&doc.id)
                    && ["capture", "tokens", "inbox", "external-clients"].contains(&f.id))
        })
        .collect();
    let view = DocumentView {
        i18n: i18n.clone(),
        features,
        doc,
        html: rendered.html,
        outline: rendered.outline,
        markdown_href: format!("docs/{}.md{suffix}", doc.id),
        current_href,
        releases,
        selected_release,
        sidebar: documentation::DOCUMENTS,
    };
    match view.render() {
        Ok(html) => response(Html(html)),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(crate) async fn api_index(Query(query): Query<DocsQuery>) -> Response {
    if query.q.chars().count() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    response(Json(
        serde_json::json!({"version":VERSION,"documents":documentation::search(&query.q)}),
    ))
}

pub(crate) async fn api_page(Path(slug): Path<String>, Query(query): Query<DocsQuery>) -> Response {
    let Some(doc) = documentation::find(&slug) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (body, release, options) = match selected(doc, query.release.as_deref()) {
        Ok(s) => s,
        Err(s) => return s.into_response(),
    };
    response(Json(
        serde_json::json!({"version":VERSION,"document":doc,"markdown":body,"release":release,"releases":options.into_iter().map(|r|r.id).collect::<Vec<_>>()}),
    ))
}

/// Safe facts only; authorization is recalculated on every request.
pub(crate) async fn api_status(
    State(state): State<Arc<WebState>>,
    auth: Option<axum::Extension<AuthLevel>>,
) -> Response {
    match crate::features::states(&state, auth.map(|v| v.0)).await {
        Ok(features) => {
            let mut r =
                Json(serde_json::json!({"version":VERSION,"features":features})).into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("private, no-store"),
            );
            r
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
