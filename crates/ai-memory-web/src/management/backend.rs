//! In-process requests preserve the existing authentication and mutation handlers.
use axum::{
    Router,
    body::Body,
    http::{Extensions, HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;
use tower::ServiceExt;

// Fifty bounded observations may include sizeable sanitized bodies.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Client {
    pub router: Router,
    pub headers: HeaderMap,
    pub extensions: Extensions,
}
pub(super) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub data: Value,
}
impl Client {
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        csrf: Option<&str>,
    ) -> Result<Reply, Response> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
        *request.headers_mut() = self.headers.clone();
        // Forward transport identity for the existing login limiter, not axum's
        // routing parameters or a previous handler's authorization extensions.
        if let Some(peer) = self
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        {
            request.extensions_mut().insert(*peer);
        }
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        request.headers_mut().remove(header::CONTENT_LENGTH);
        if let Some(csrf) = csrf {
            request.headers_mut().insert(
                "x-csrf-token",
                csrf.parse()
                    .map_err(|_| StatusCode::BAD_REQUEST.into_response())?,
            );
        }
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .map_err(|never| match never {})?;
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, MAX_RESPONSE_BYTES)
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY.into_response())?;
        Ok(Reply {
            status: parts.status,
            headers: parts.headers,
            data: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        })
    }
    pub async fn get(&self, path: &str) -> Result<Value, Response> {
        let reply = self.call("GET", path, None, None).await?;
        if reply.status.is_success() {
            Ok(reply.data)
        } else {
            Err(super::failure(
                &self
                    .extensions
                    .get::<ai_memory_core::i18n::Localizer>()
                    .cloned()
                    .unwrap_or_default(),
                reply.status,
                super::views::text(&reply.data, "error"),
            ))
        }
    }
}

pub(super) fn csrf_cookie(headers: &HeaderMap) -> String {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("ai_memory_csrf="))
        .unwrap_or("")
        .to_string()
}

pub(super) fn copy_cookies(headers: &HeaderMap, response: &mut Response) {
    for cookie in headers.get_all(header::SET_COOKIE) {
        response
            .headers_mut()
            .append(header::SET_COOKIE, cookie.clone());
    }
}
