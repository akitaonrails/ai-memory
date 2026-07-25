//! Bearer-token resolution shared by lifecycle hooks and thin HTTP clients.
//!
//! Static CLI/config tokens always win. When they are absent, a stored OIDC
//! device-flow token is loaded from `auth.json`, refreshed if stale, and used
//! as the bearer for server HTTP calls.

use std::path::Path;

use ai_memory_llm::{OidcToken, refresh_access_token};
use secrecy::ExposeSecret as _;

/// Resolve the bearer for one server request: explicit/static token first,
/// stored OIDC token second, and no token last.
pub async fn resolve_bearer(
    client: &reqwest::Client,
    auth_path: &Path,
    static_token: Option<&str>,
) -> Option<String> {
    match static_token.filter(|t| !t.is_empty()) {
        Some(t) => Some(t.to_string()),
        None => resolve_oidc(client, auth_path).await,
    }
}

/// Load the stored OIDC token, refreshing and persisting it when stale.
///
/// Returns the access token, or `None` when there is no token. Refresh failures
/// fall back to the existing token because it may still be inside the provider's
/// accepted clock-skew window.
pub async fn resolve_oidc(client: &reqwest::Client, auth_path: &Path) -> Option<String> {
    let mut token = OidcToken::load(auth_path).ok().flatten()?;
    if token.needs_refresh() {
        let Ok(refreshed) = refresh_access_token(client, &token).await else {
            return Some(token.access.expose_secret().to_string());
        };
        if refreshed.save(auth_path).is_err() {
            // Avoid using a rotated token that was not persisted; keep this
            // request aligned with the still-current auth.json state.
            return Some(token.access.expose_secret().to_string());
        }
        token = refreshed;
    }
    Some(token.access.expose_secret().to_string())
}

#[cfg(test)]
mod tests {
    use ai_memory_llm::OidcExtras;

    use super::*;

    use secrecy::SecretString;

    fn oidc_token(endpoint: &str, access: &str, expires_at_ms: u64) -> OidcToken {
        OidcToken {
            access: SecretString::from(access.to_string()),
            refresh: SecretString::from("refresh-token".to_string()),
            expires_at_ms,
            extra: OidcExtras {
                issuer: "https://issuer.example.com/realms/team".to_string(),
                client_id: "ai-memory-cli".to_string(),
                token_endpoint: endpoint.to_string(),
            },
        }
    }

    fn save_oidc_token(path: &Path, access: &str) {
        let token = oidc_token("https://issuer.example.com/token", access, u64::MAX);
        token.save(path).expect("save test OIDC token");
    }

    fn save_refreshing_oidc_token(path: &Path, endpoint: &str, access: &str) {
        let token = oidc_token(endpoint, access, 0);
        token.save(path).expect("save test OIDC token");
    }

    async fn serve_refresh(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/token")
    }

    #[tokio::test]
    async fn static_token_wins_over_stored_oidc() {
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.json");
        save_oidc_token(&auth_path, "oidc-access");
        let client = reqwest::Client::new();

        let bearer = resolve_bearer(&client, &auth_path, Some("static-token")).await;

        assert_eq!(bearer.as_deref(), Some("static-token"));
    }

    #[tokio::test]
    async fn stored_oidc_is_used_when_static_token_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.json");
        save_oidc_token(&auth_path, "oidc-access");
        let client = reqwest::Client::new();

        let bearer = resolve_bearer(&client, &auth_path, None).await;

        assert_eq!(bearer.as_deref(), Some("oidc-access"));
    }

    #[tokio::test]
    async fn empty_when_static_and_oidc_are_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.json");
        let client = reqwest::Client::new();

        let bearer = resolve_bearer(&client, &auth_path, None).await;

        assert!(bearer.is_none());
    }

    #[tokio::test]
    async fn refresh_failure_falls_back_to_existing_token() {
        let endpoint = serve_refresh("401 Unauthorized", r#"{"error":"invalid_grant"}"#).await;
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.json");
        save_refreshing_oidc_token(&auth_path, &endpoint, "old-access");
        let client = reqwest::Client::new();

        let bearer = resolve_bearer(&client, &auth_path, None).await;

        assert_eq!(bearer.as_deref(), Some("old-access"));
    }

    #[tokio::test]
    async fn refresh_success_persists_and_uses_refreshed_token() {
        let endpoint = serve_refresh(
            "200 OK",
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":300}"#,
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.json");
        save_refreshing_oidc_token(&auth_path, &endpoint, "old-access");
        let client = reqwest::Client::new();

        let bearer = resolve_bearer(&client, &auth_path, None).await;
        let saved = OidcToken::load(&auth_path).unwrap().unwrap();

        assert_eq!(bearer.as_deref(), Some("new-access"));
        assert_eq!(saved.access.expose_secret(), "new-access");
        assert_eq!(saved.refresh.expose_secret(), "new-refresh");
    }

    #[tokio::test]
    async fn refresh_save_failure_falls_back_to_existing_token() {
        let endpoint = serve_refresh(
            "200 OK",
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":300}"#,
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.json");
        save_refreshing_oidc_token(&auth_path, &endpoint, "old-access");
        std::fs::create_dir(auth_path.with_extension("json.tmp")).unwrap();
        let client = reqwest::Client::new();

        let bearer = resolve_bearer(&client, &auth_path, None).await;
        let saved = OidcToken::load(&auth_path).unwrap().unwrap();

        assert_eq!(bearer.as_deref(), Some("old-access"));
        assert_eq!(saved.access.expose_secret(), "old-access");
    }
}
