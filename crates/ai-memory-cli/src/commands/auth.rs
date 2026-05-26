//! `ai-memory auth` — OAuth authentication for LLM providers.
//!
//! OpenAI (`login openai` / `logout openai`):
//!   Opens the user's browser to `auth.openai.com`, listens on
//!   `127.0.0.1:1455` for the redirect, exchanges the authorization code for
//!   tokens, and writes them to `<data_dir>/oauth_token.json` (mode 0600).
//!
//! GitHub Copilot (`login copilot` / `login copilot`):
//!   Runs the RFC 8628 device flow — displays a short code, polls GitHub
//!   until the user approves, and writes the GitHub OAuth token to
//!   `<data_dir>/oauth_token.json` (mode 0600).
//!
//! All subcommands are pre-server local operations (same exception class as
//! `init`, `generate-auth-token`) per CLAUDE.md rule 16.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::info;

use ai_memory_llm::copilot::{COPILOT_CLIENT_ID, COPILOT_SCOPE, CopilotToken};
use ai_memory_llm::openai_oauth::{CODEX_CLIENT_ID, OAUTH_SCOPES, OAuthToken, TOKEN_URL};
use ai_memory_llm::token_store::TokenFile;

use crate::cli::{AuthLoginArgs, AuthLogoutArgs, AuthProvider, AuthSubcommand};
use crate::config::Config;

type CallbackState = Arc<Mutex<Option<oneshot::Sender<Result<(String, String), String>>>>>;

const CALLBACK_PORT: u16 = 1455;
const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";

#[derive(Serialize)]
struct Body<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
}

#[derive(Deserialize)]
struct Response {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Dispatch `auth login` or `auth logout`.
///
/// # Errors
/// Propagates IO, HTTP, or OAuth protocol errors.
pub async fn run(config: &Config, sub: AuthSubcommand) -> Result<()> {
    // Both providers share one file; only the key inside changes.
    let token_path = config.data_dir.join("oauth_token.json");
    match sub {
        AuthSubcommand::Login(AuthLoginArgs { provider }) => match provider {
            AuthProvider::OpenAi => run_login(&token_path).await,
            AuthProvider::Copilot => run_copilot_login(&token_path).await,
        },
        AuthSubcommand::Logout(AuthLogoutArgs { provider }) => match provider {
            AuthProvider::OpenAi => run_logout_openai(&token_path),
            AuthProvider::Copilot => run_logout_copilot(&token_path),
        },
    }
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

async fn run_login(token_path: &Path) -> Result<()> {
    // PKCE: generate code_verifier (64 random bytes, base64url-encoded → 86 chars)
    let mut verifier_bytes = [0u8; 64];
    getrandom::fill(&mut verifier_bytes)
        .map_err(|e| anyhow::anyhow!("generate PKCE code verifier: {e}"))?;
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);

    // code_challenge = BASE64URL(SHA256(code_verifier))
    let challenge_hash = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge_hash);

    // Random state for CSRF protection
    let mut state_bytes = [0u8; 32];
    getrandom::fill(&mut state_bytes).map_err(|e| anyhow::anyhow!("generate OAuth state: {e}"))?;
    let expected_state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state_bytes);

    let redirect_uri = format!("http://localhost:{CALLBACK_PORT}/auth/callback");

    let auth_url = build_auth_url(&redirect_uri, &code_challenge, &expected_state);

    // Bind first so we can give a clear error before opening the browser.
    let listener = TcpListener::bind(format!("127.0.0.1:{CALLBACK_PORT}"))
        .await
        .with_context(|| {
            format!(
                "port {CALLBACK_PORT} is already in use.\n\
                 Another process holds the OAuth callback port.\n\
                 Free it or forward over SSH: ssh -L {CALLBACK_PORT}:localhost:{CALLBACK_PORT} user@host"
            )
        })?;

    // Channel: callback handler → here.
    let (code_tx, code_rx) = oneshot::channel::<Result<(String, String), String>>();
    let sender: CallbackState = Arc::new(Mutex::new(Some(code_tx)));

    let app = Router::new()
        .route("/auth/callback", get(callback_handler))
        .with_state(sender);

    // Open browser (best-effort; print URL as fallback).
    println!("Opening browser for OpenAI authentication...");
    if open::that(&auth_url).is_err() {
        println!(
            "Could not open browser automatically.\nVisit this URL manually:\n\n  {auth_url}\n"
        );
    } else {
        println!("Browser opened. Waiting for callback on port {CALLBACK_PORT}...");
    }

    // Serve until the callback fires, then abort the task.
    let serve_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let callback_result = code_rx
        .await
        .context("callback server closed before receiving the authorization code")?;

    serve_handle.abort();

    let (code, returned_state) =
        callback_result.map_err(|e| anyhow::anyhow!("OAuth error from provider: {e}"))?;

    if returned_state != expected_state {
        bail!("OAuth state mismatch — possible CSRF attack; aborting login");
    }

    info!("authorization code received, exchanging for tokens");
    let token = exchange_code(&code, &redirect_uri, &code_verifier).await?;

    token
        .save(token_path)
        .map_err(|e| anyhow::anyhow!("save token: {e}"))?;

    println!(
        "Authentication successful. Token saved to {}",
        token_path.display()
    );
    println!("Set AI_MEMORY_LLM_PROVIDER=openai-oauth to use your ChatGPT subscription.");
    Ok(())
}

fn run_logout_openai(token_path: &Path) -> Result<()> {
    let mut file =
        TokenFile::load(token_path).map_err(|e| anyhow::anyhow!("load token file: {e}"))?;
    if file.openai.is_none() {
        println!("Not logged in to OpenAI (no token found).");
        return Ok(());
    }
    file.openai = None;
    clear_or_delete(&file, token_path)?;
    println!("Logged out from OpenAI. Token removed.");
    Ok(())
}

fn build_auth_url(redirect_uri: &str, code_challenge: &str, state: &str) -> String {
    format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code\
         &scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        percent_encode(CODEX_CLIENT_ID),
        percent_encode(redirect_uri),
        percent_encode(OAUTH_SCOPES),
        percent_encode(code_challenge),
        percent_encode(state),
    )
}

/// Minimal percent-encoding for OAuth query-string values (RFC 3986 §2.3).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

async fn callback_handler(
    State(tx): State<CallbackState>,
    Query(params): Query<CallbackParams>,
) -> Html<&'static str> {
    let mut guard = tx.lock().await;
    let Some(sender) = guard.take() else {
        // Duplicate callback (e.g. browser retry). Ignore.
        return Html(SUCCESS_PAGE);
    };

    if let Some(err) = params.error {
        let desc = params
            .error_description
            .unwrap_or_else(|| "(no description)".into());
        let _ = sender.send(Err(format!("{err}: {desc}")));
        return Html(FAILURE_PAGE);
    }

    if let (Some(code), Some(state)) = (params.code, params.state) {
        let _ = sender.send(Ok((code, state)));
        Html(SUCCESS_PAGE)
    } else {
        let _ = sender.send(Err("missing code or state parameter".into()));
        Html(FAILURE_PAGE)
    }
}

const SUCCESS_PAGE: &str = "<!DOCTYPE html><html><head>\
    <title>ai-memory — authenticated</title></head><body>\
    <h1>Authentication successful</h1>\
    <p>You may close this tab and return to the terminal.</p>\
    </body></html>";

const FAILURE_PAGE: &str = "<!DOCTYPE html><html><head>\
    <title>ai-memory — authentication failed</title></head><body>\
    <h1>Authentication failed</h1>\
    <p>Check the terminal for details. You may close this tab.</p>\
    </body></html>";

async fn exchange_code(code: &str, redirect_uri: &str, code_verifier: &str) -> Result<OAuthToken> {
    let client = reqwest::Client::new();
    let body = Body {
        grant_type: "authorization_code",
        client_id: CODEX_CLIENT_ID,
        code,
        redirect_uri,
        code_verifier,
    };

    let resp = client
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await
        .context("POST to token endpoint")?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("token exchange failed ({status}): {text}");
    }

    let r: Response = resp.json().await.context("parse token response")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(OAuthToken {
        access_token: r.access_token,
        refresh_token: r
            .refresh_token
            .context("token response missing refresh_token (was offline_access scope granted?)")?,
        expires_at: now + r.expires_in,
        token_type: r.token_type,
        scope: r.scope.unwrap_or_else(|| OAUTH_SCOPES.to_string()),
        account_id: None,
    })
}

async fn run_copilot_login(token_path: &Path) -> Result<()> {
    let client = reqwest::Client::new();

    #[derive(Serialize)]
    struct DeviceCodeReq<'a> {
        client_id: &'a str,
        scope: &'a str,
    }

    #[derive(Deserialize)]
    struct DeviceCodeResp {
        device_code: String,
        user_code: String,
        verification_uri: String,
        expires_in: u64,
        interval: u64,
    }

    let device_resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .json(&DeviceCodeReq {
            client_id: COPILOT_CLIENT_ID,
            scope: COPILOT_SCOPE,
        })
        .send()
        .await
        .context("POST GitHub device code")?;

    if !device_resp.status().is_success() {
        let text = device_resp.text().await.unwrap_or_default();
        bail!("device code request failed: {text}");
    }

    let device: DeviceCodeResp = device_resp
        .json()
        .await
        .context("parse device code response")?;

    println!("\nVisit this URL to authenticate with GitHub:");
    println!("  {}", device.verification_uri);
    println!("\nEnter code: {}", device.user_code);
    println!("\nWaiting for you to authorize in the browser...");

    // Best-effort browser open — fall through silently on failure.
    let _ = open::that(&device.verification_uri);

    #[derive(Serialize)]
    struct TokenPollReq<'a> {
        client_id: &'a str,
        device_code: &'a str,
        grant_type: &'a str,
    }

    #[derive(Deserialize)]
    struct TokenPollResp {
        access_token: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }

    let mut poll_interval = device.interval;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;

        if std::time::Instant::now() > deadline {
            bail!("device code expired before authorization was granted");
        }

        let poll_resp = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .json(&TokenPollReq {
                client_id: COPILOT_CLIENT_ID,
                device_code: &device.device_code,
                grant_type: "urn:ietf:params:oauth:grant-type:device_code",
            })
            .send()
            .await
            .context("POST GitHub device token poll")?;

        let poll: TokenPollResp = poll_resp
            .json()
            .await
            .context("parse device token poll response")?;

        if let Some(token) = poll.access_token {
            let copilot_token = CopilotToken {
                github_token: token,
            };
            copilot_token
                .save(token_path)
                .map_err(|e| anyhow::anyhow!("save copilot token: {e}"))?;
            println!(
                "\nAuthentication successful. Copilot token saved to {}",
                token_path.display()
            );
            println!("Set AI_MEMORY_LLM_PROVIDER=copilot to use your GitHub Copilot subscription.");
            return Ok(());
        }

        match poll.error.as_deref() {
            Some("authorization_pending") => {
                // Normal — keep polling at the current interval.
            }
            Some("slow_down") => {
                // Server asked us to back off — add 5 s and keep going.
                poll_interval += 5;
            }
            Some("expired_token") => {
                bail!("device code expired — run `ai-memory auth login copilot` to try again");
            }
            Some("access_denied") => {
                bail!("authorization was denied by the user");
            }
            Some(err) => {
                let desc = poll
                    .error_description
                    .unwrap_or_else(|| "(no description)".into());
                bail!("OAuth error from GitHub: {err}: {desc}");
            }
            None => {
                bail!("unexpected empty response from GitHub token endpoint");
            }
        }
    }
}

fn run_logout_copilot(token_path: &Path) -> Result<()> {
    let mut file =
        TokenFile::load(token_path).map_err(|e| anyhow::anyhow!("load token file: {e}"))?;
    if file.copilot.is_none() {
        println!("Not logged in to GitHub Copilot (no token found).");
        return Ok(());
    }
    file.copilot = None;
    clear_or_delete(&file, token_path)?;
    println!("Logged out from GitHub Copilot. Token removed.");
    Ok(())
}

fn clear_or_delete(file: &TokenFile, path: &Path) -> Result<()> {
    if file.is_empty() {
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("remove token file {}", path.display()))?;
        }
    } else {
        file.save(path)
            .map_err(|e| anyhow::anyhow!("save token file: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn percent_encode_leaves_unreserved_chars() {
        assert_eq!(percent_encode("abc-123_OK.~"), "abc-123_OK.~");
    }

    #[test]
    fn percent_encode_encodes_space_and_colon() {
        let encoded = percent_encode("a b:c");
        assert_eq!(encoded, "a%20b%3Ac");
    }

    #[test]
    fn percent_encode_encodes_slashes() {
        let encoded = percent_encode("http://localhost:1455/cb");
        assert_eq!(encoded, "http%3A%2F%2Flocalhost%3A1455%2Fcb");
    }
}
