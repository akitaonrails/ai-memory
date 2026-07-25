//! `ai-memory auth` — manage upstream LLM provider credentials.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ai_memory_llm::{
    CODEX_CLIENT_ID, CopilotToken, DeviceAuthorizationResponse, GITHUB_ACCESS_TOKEN_URL,
    GITHUB_COPILOT_CLIENT_ID, GITHUB_DEVICE_CODE_URL, OIDC_DEFAULT_SCOPE, OPENAI_OAUTH_TOKEN_URL,
    OidcDiscovery, OidcToken, OidcTokenResponse, OpenAiOAuthToken, OpenAiOAuthTokenResponse,
    PollOutcome, discover, poll_token_once, request_device_code,
};
use anyhow::{Context, Result, anyhow, bail};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::cli::{AuthArgs, AuthCommand, AuthProviderChoice};
use crate::config::Config;

const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_BROWSER_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const POLLING_SAFETY_MARGIN_SECS: u64 = 3;

/// Run the `auth` subcommand.
///
/// # Errors
/// Returns an error if token storage or provider auth requests fail.
pub async fn run(config: &Config, args: AuthArgs) -> Result<()> {
    match args.command {
        AuthCommand::Login(args) => match args.provider {
            AuthProviderChoice::OpenaiOauth => {
                if args.github_token.is_some() || args.client_id.is_some() || args.issuer.is_some()
                {
                    bail!(
                        "--github-token / --client-id / --issuer do not apply to `auth login openai-oauth`"
                    );
                }
                login_openai_oauth(config, args.timeout_secs).await
            }
            AuthProviderChoice::Copilot => {
                if args.issuer.is_some() {
                    bail!("--issuer applies only to `auth login oidc-device`");
                }
                login_copilot(config, args.timeout_secs, args.github_token, args.client_id).await
            }
            AuthProviderChoice::OidcDevice => {
                if args.github_token.is_some() {
                    bail!("--github-token applies only to `auth login copilot`");
                }
                let issuer = args
                    .issuer
                    .context("--issuer is required for `auth login oidc-device`")?;
                let client_id = args
                    .client_id
                    .context("--client-id is required for `auth login oidc-device`")?;
                login_oidc_device(config, &issuer, &client_id, args.timeout_secs).await
            }
        },
        AuthCommand::Logout(args) => match args.provider {
            AuthProviderChoice::OpenaiOauth => logout_openai_oauth(config),
            AuthProviderChoice::Copilot => logout_copilot(config),
            AuthProviderChoice::OidcDevice => logout_oidc_device(config),
        },
        AuthCommand::Status(_) => status(config),
    }
}

async fn login_oidc_device(
    config: &Config,
    issuer: &str,
    client_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    let client = auth_http_client()?;
    let discovery = discover(&client, issuer).await?;
    let device = request_device_code(&client, &discovery, client_id, OIDC_DEFAULT_SCOPE).await?;

    match device.verification_uri_complete.as_deref() {
        Some(complete) => println!("Open this URL: {complete}"),
        None => {
            println!("Open this URL: {}", device.verification_uri);
            println!("Enter code: {}", device.user_code);
        }
    }
    println!("Waiting for authorization...");

    let token_response = poll_oidc_device(
        &client,
        &discovery,
        client_id,
        &device,
        Duration::from_secs(timeout_secs),
    )
    .await?;
    let token = OidcToken::from_token_response(
        &token_response,
        issuer,
        client_id,
        &discovery.token_endpoint,
        None,
    )?;
    let path = config.oidc_device_token_path();
    token.save(&path).map_err(anyhow::Error::from)?;

    println!("oidc-device: logged in");
    println!("issuer: {issuer}");
    println!("token file: {}", path.display());
    Ok(())
}

async fn poll_oidc_device(
    client: &reqwest::Client,
    discovery: &OidcDiscovery,
    client_id: &str,
    device: &DeviceAuthorizationResponse,
    timeout: Duration,
) -> Result<OidcTokenResponse> {
    let started = Instant::now();
    let mut interval = Duration::from_secs(
        device
            .interval
            .unwrap_or(5)
            .max(1)
            .saturating_add(POLLING_SAFETY_MARGIN_SECS),
    );
    let device_timeout = Duration::from_secs(device.expires_in.unwrap_or(600).max(1));
    let timeout = timeout.min(device_timeout);
    loop {
        if started.elapsed() >= timeout {
            bail!("timed out waiting for oidc-device authorization");
        }
        match poll_token_once(client, discovery, client_id, &device.device_code).await? {
            PollOutcome::Token(token) => return Ok(*token),
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval = interval.saturating_add(Duration::from_secs(5)),
            PollOutcome::Denied => bail!("oidc-device authorization denied"),
            PollOutcome::Expired => bail!("oidc-device code expired before authorization"),
            PollOutcome::Other(error) => bail!("oidc-device authorization failed: {error}"),
        }
        sleep(interval).await;
    }
}

fn logout_oidc_device(config: &Config) -> Result<()> {
    let path = config.oidc_device_token_path();
    OidcToken::remove(&path).map_err(anyhow::Error::from)?;
    println!("oidc-device: logged out");
    println!("token file: {}", path.display());
    Ok(())
}

async fn login_openai_oauth(config: &Config, timeout_secs: u64) -> Result<()> {
    let client = auth_http_client()?;
    let device = start_device_authorization(&client).await?;
    println!("Open this URL: {DEVICE_BROWSER_URL}");
    println!("Enter code: {}", device.user_code);
    println!("Waiting for authorization...");

    let code =
        poll_device_authorization(&client, &device, Duration::from_secs(timeout_secs)).await?;
    let tokens = exchange_authorization_code(&client, code).await?;
    let refresh = tokens.refresh_token.clone().ok_or_else(|| {
        anyhow::anyhow!("openai-oauth token response did not include refresh_token")
    })?;
    let token = OpenAiOAuthToken::from_token_response(
        tokens.access_token,
        refresh,
        tokens.expires_in.unwrap_or(3600),
        tokens.id_token.as_deref(),
        None,
    );
    let path = config.openai_oauth_token_path();
    token.save(&path).map_err(anyhow::Error::from)?;

    println!("openai-oauth: logged in");
    if let Some(account_id) = token.extra.account_id.as_deref() {
        println!("account: {account_id}");
    }
    println!("token file: {}", path.display());
    Ok(())
}

fn logout_openai_oauth(config: &Config) -> Result<()> {
    let path = config.openai_oauth_token_path();
    OpenAiOAuthToken::remove(&path).map_err(anyhow::Error::from)?;
    println!("openai-oauth: logged out");
    println!("token file: {}", path.display());
    Ok(())
}

async fn login_copilot(
    config: &Config,
    timeout_secs: u64,
    github_token_arg: Option<String>,
    client_id_arg: Option<String>,
) -> Result<()> {
    let github_token = match github_token_arg.filter(|s| !s.trim().is_empty()) {
        Some(token) => token,
        None => match config.copilot_github_token() {
            Some(token) => token.expose_secret().to_string(),
            None => {
                let client_id = client_id_arg
                    .as_deref()
                    .or_else(|| config.copilot_client_id())
                    .unwrap_or(GITHUB_COPILOT_CLIENT_ID);
                run_copilot_device_flow(client_id, timeout_secs).await?
            }
        },
    };

    let token = CopilotToken::from_github_token(github_token, None);
    let path = config.copilot_token_path();
    token.save(&path).map_err(anyhow::Error::from)?;
    println!("copilot: logged in");
    println!("token file: {}", path.display());
    Ok(())
}

fn logout_copilot(config: &Config) -> Result<()> {
    let path = config.copilot_token_path();
    CopilotToken::remove(&path).map_err(anyhow::Error::from)?;
    println!("copilot: logged out");
    println!("token file: {}", path.display());
    Ok(())
}

fn status(config: &Config) -> Result<()> {
    let openai_path = config.openai_oauth_token_path();
    match OpenAiOAuthToken::load(&openai_path).map_err(anyhow::Error::from)? {
        Some(token) => {
            println!("openai-oauth: logged in");
            if let Some(account_id) = token.extra.account_id.as_deref() {
                println!("account: {account_id}");
            }
            println!("expires in: {}", format_duration_until(token.expires_at_ms));
            println!("token file: {}", openai_path.display());
        }
        None => {
            println!("openai-oauth: not logged in");
            println!("token file: {}", openai_path.display());
        }
    }

    let copilot_path = config.copilot_token_path();
    match CopilotToken::load(&copilot_path).map_err(anyhow::Error::from)? {
        Some(token) => {
            let has_refreshable_github = token.has_refreshable_github_token();
            let has_valid_cached = token.has_valid_cached_copilot_token();
            if has_refreshable_github {
                println!("copilot: logged in");
            } else if has_valid_cached {
                println!("copilot: cached token valid (no GitHub token stored for refresh)");
            } else {
                println!(
                    "copilot: not logged in (cached token expired and no refreshable GitHub token stored)"
                );
            }
            if let Some(expires_at_ms) = token.github_expires_at_ms {
                println!(
                    "github token expires in: {}",
                    format_duration_until(expires_at_ms)
                );
            }
            if let Some(expires_at_ms) = token.copilot_expires_at_ms {
                println!(
                    "cached copilot token expires in: {}",
                    format_duration_until(expires_at_ms)
                );
            }
            if let Some(api_base_url) = token.api_base_url.as_deref() {
                println!("api base: {api_base_url}");
            }
            println!("token file: {}", copilot_path.display());
        }
        None => {
            println!("copilot: not logged in");
            println!("token file: {}", copilot_path.display());
        }
    }

    let oidc_path = config.oidc_device_token_path();
    match OidcToken::load(&oidc_path).map_err(anyhow::Error::from)? {
        Some(token) => {
            println!("oidc-device: logged in");
            println!("issuer: {}", token.extra.issuer);
            println!("expires in: {}", format_duration_until(token.expires_at_ms));
            println!("token file: {}", oidc_path.display());
        }
        None => {
            println!("oidc-device: not logged in");
            println!("token file: {}", oidc_path.display());
        }
    }
    Ok(())
}

async fn run_copilot_device_flow(client_id: &str, timeout_secs: u64) -> Result<String> {
    let client = auth_http_client()?;
    let device = start_github_device_authorization(&client, client_id).await?;
    println!("Open this URL: {}", device.verification_uri);
    println!("Enter code: {}", device.user_code);
    println!("Waiting for authorization...");
    poll_github_device_authorization(
        &client,
        client_id,
        &device,
        Duration::from_secs(timeout_secs),
    )
    .await
}

fn auth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(format!("ai-memory/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

async fn start_device_authorization(client: &reqwest::Client) -> Result<DeviceAuthorization> {
    let resp = client
        .post(DEVICE_USER_CODE_URL)
        .json(&DeviceAuthorizationRequest {
            client_id: CODEX_CLIENT_ID,
        })
        .send()
        .await
        .context("starting openai-oauth device authorization")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("openai-oauth device authorization failed ({status}): {body}");
    }
    resp.json::<DeviceAuthorization>()
        .await
        .context("parsing openai-oauth device authorization response")
}

async fn poll_device_authorization(
    client: &reqwest::Client,
    device: &DeviceAuthorization,
    timeout: Duration,
) -> Result<DeviceAuthorizationCode> {
    let started = Instant::now();
    let interval = Duration::from_secs(
        device
            .interval
            .parse::<u64>()
            .unwrap_or(5)
            .max(1)
            .saturating_add(POLLING_SAFETY_MARGIN_SECS),
    );
    loop {
        if started.elapsed() >= timeout {
            bail!("timed out waiting for openai-oauth authorization");
        }
        let resp = client
            .post(DEVICE_TOKEN_URL)
            .json(&DeviceTokenRequest {
                device_auth_id: &device.device_auth_id,
                user_code: &device.user_code,
            })
            .send()
            .await
            .context("polling openai-oauth device authorization")?;
        if resp.status().is_success() {
            return resp
                .json::<DeviceAuthorizationCode>()
                .await
                .context("parsing openai-oauth authorization code");
        }
        if resp.status() != reqwest::StatusCode::FORBIDDEN
            && resp.status() != reqwest::StatusCode::NOT_FOUND
        {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("openai-oauth authorization polling failed ({status}): {body}");
        }
        sleep(interval).await;
    }
}

async fn exchange_authorization_code(
    client: &reqwest::Client,
    code: DeviceAuthorizationCode,
) -> Result<OpenAiOAuthTokenResponse> {
    let resp = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.authorization_code.as_str()),
            ("redirect_uri", DEVICE_REDIRECT_URI),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", code.code_verifier.as_str()),
        ])
        .send()
        .await
        .context("exchanging openai-oauth authorization code")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("openai-oauth token exchange failed ({status}): {body}");
    }
    resp.json::<OpenAiOAuthTokenResponse>()
        .await
        .context("parsing openai-oauth token response")
}

async fn start_github_device_authorization(
    client: &reqwest::Client,
    client_id: &str,
) -> Result<GitHubDeviceAuthorization> {
    let resp = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[("client_id", client_id), ("scope", "read:user")])
        .send()
        .await
        .context("starting copilot GitHub device authorization")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("copilot GitHub device authorization failed ({status}): {body}");
    }
    resp.json::<GitHubDeviceAuthorization>()
        .await
        .context("parsing copilot GitHub device authorization response")
}

async fn poll_github_device_authorization(
    client: &reqwest::Client,
    client_id: &str,
    device: &GitHubDeviceAuthorization,
    timeout: Duration,
) -> Result<String> {
    let started = Instant::now();
    let mut interval = Duration::from_secs(device.interval.unwrap_or(5).max(1));
    let device_timeout = Duration::from_secs(device.expires_in.max(1));
    let timeout = timeout.min(device_timeout);
    loop {
        if started.elapsed() >= timeout {
            bail!("timed out waiting for copilot GitHub authorization");
        }
        let resp = client
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .context("polling copilot GitHub device authorization")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("copilot GitHub authorization polling failed ({status}): {body}");
        }
        let body = resp
            .json::<GitHubDeviceAccessToken>()
            .await
            .context("parsing copilot GitHub token response")?;
        if let Some(token) = body.access_token {
            return Ok(token);
        }
        match body.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = interval.saturating_add(Duration::from_secs(5)),
            Some("expired_token") => bail!("copilot GitHub device code expired"),
            Some("access_denied") => bail!("copilot GitHub authorization denied"),
            Some(error) => {
                let description = body.error_description.unwrap_or_default();
                return Err(anyhow!(
                    "copilot GitHub authorization failed: {error} {description}"
                ));
            }
            None => bail!("copilot GitHub token response did not include access_token"),
        }
        sleep(interval).await;
    }
}

fn format_duration_until(expires_at_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    if expires_at_ms <= now {
        return "expired".into();
    }
    let secs = (expires_at_ms - now) / 1000;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[derive(Debug, Serialize)]
struct DeviceAuthorizationRequest {
    client_id: &'static str,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Debug, Serialize)]
struct DeviceTokenRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationCode {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct GitHubDeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubDeviceAccessToken {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_status_does_not_print_exact_token_time() {
        assert_eq!(format_duration_until(0), "expired");
    }
}
