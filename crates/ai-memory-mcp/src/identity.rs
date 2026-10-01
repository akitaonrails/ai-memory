//! Machine identity introspection, mounted behind the machine auth middleware.

use std::sync::Arc;

use ai_memory_core::{ActorContext, AuthLevel, Capability};
use ai_memory_store::ReaderPool;
use axum::{
    Extension, Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

#[derive(Clone)]
struct IdentityState {
    reader: ReaderPool,
    trusted_proxy_identity: bool,
}

/// Build the always-mounted machine identity router.
/// The caller must apply `require_bearer`, just as for MCP and hooks.
pub fn router(reader: ReaderPool, trusted_proxy_identity: bool) -> Router {
    Router::new()
        .route("/identity", get(identity))
        .with_state(Arc::new(IdentityState {
            reader,
            trusted_proxy_identity,
        }))
}

#[derive(Serialize)]
struct IdentityResponse {
    version: &'static str,
    level: &'static str,
    operator: Option<String>,
    distinguishes_operators: bool,
}

async fn identity(
    State(state): State<Arc<IdentityState>>,
    Extension(level): Extension<AuthLevel>,
    Extension(actor): Extension<ActorContext>,
) -> Result<Response, StatusCode> {
    let distinguishes_operators = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    level
        .authorize(Capability::NormalRead, distinguishes_operators)
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let body = IdentityResponse {
        version: env!("CARGO_PKG_VERSION"),
        level: match level {
            AuthLevel::Anonymous => "anonymous",
            AuthLevel::User => "user",
            AuthLevel::Root => "root",
        },
        operator: actor.identity_key().map(|key| key.storage_key()),
        distinguishes_operators,
    };
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthState, require_bearer};
    use ai_memory_core::{ApiCredentialId, NewUser, UserRole};
    use ai_memory_store::{Store, TokenPepper};
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    async fn ask(app: Router, token: Option<&str>) -> Response {
        let mut req = Request::builder()
            .uri("/identity")
            .header("x-memory-actor-user", "forged");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn identity_real_auth_isolates_users_root_and_revoked_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let pepper = TokenPepper::new("identity-test");
        let state = AuthState::new(Some("root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root".into()),
                ..ActorContext::default()
            })
            .with_multiuser(pepper.clone(), store.reader.clone(), store.writer.clone());
        let app = router(store.reader.clone(), false).layer(axum::middleware::from_fn_with_state(
            Arc::new(state),
            require_bearer,
        ));
        let mut credentials = Vec::new();
        for name in ["alice", "bob"] {
            let user = store
                .writer
                .create_human_user(
                    NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap();
            let id = ApiCredentialId::new();
            let token = format!("identity-{name}-token");
            store
                .writer
                .create_api_credential(
                    id,
                    user,
                    "test".into(),
                    ai_memory_store::hash_token(&token, &pepper),
                    None,
                )
                .await
                .unwrap();
            credentials.push((id, token));
        }
        for (token, level, name) in [
            (credentials[0].1.as_str(), "user", "alice"),
            (credentials[1].1.as_str(), "user", "bob"),
            ("root-token", "root", "root"),
        ] {
            let response = ask(app.clone(), Some(token)).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, no-store"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["level"], level);
            assert_eq!(
                body["operator"],
                ai_memory_core::IdentityKey::User(name.into()).storage_key()
            );
            assert_eq!(body["distinguishes_operators"], true);
            assert_eq!(body.as_object().unwrap().len(), 4);
        }
        assert_eq!(
            ask(app.clone(), None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ask(app.clone(), Some("invalid")).await.status(),
            StatusCode::UNAUTHORIZED
        );
        store
            .writer
            .revoke_api_credential(credentials[0].0)
            .await
            .unwrap();
        assert_eq!(
            ask(app, Some(&credentials[0].1)).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn identity_anonymous_ignores_forged_headers_and_proxy_qualifies_oidc() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let app = router(store.reader.clone(), false).layer(axum::middleware::from_fn_with_state(
            Arc::new(AuthState::new(None)),
            require_bearer,
        ));
        let response = ask(app, None).await;
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["level"], "anonymous");
        assert!(body["operator"].is_null());
        assert_eq!(body["distinguishes_operators"], false);
        let app = router(store.reader.clone(), true).layer(axum::middleware::from_fn_with_state(
            Arc::new(AuthState::new(Some("root".into())).with_trusted_proxy_bearer("proxy")),
            require_bearer,
        ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/identity")
                    .header("authorization", "Bearer proxy")
                    .header("x-memory-actor-issuer", "https://issuer.example")
                    .header("x-memory-actor-sub", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["operator"],
            ai_memory_core::IdentityKey::Subject {
                issuer: "https://issuer.example".into(),
                subject: "alice".into()
            }
            .storage_key()
        );
        assert_eq!(body["distinguishes_operators"], true);
    }
}
