//! `ai-memory auth` — manage upstream LLM provider credentials, and require a
//! bearer token on this install's own server (`auth enable`).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ai_memory_llm::{
    CODEX_CLIENT_ID, CopilotToken, DeviceAuthorizationResponse, GITHUB_ACCESS_TOKEN_URL,
    GITHUB_COPILOT_CLIENT_ID, GITHUB_DEVICE_CODE_URL, OIDC_DEFAULT_SCOPE, OPENAI_OAUTH_TOKEN_URL,
    OidcDiscovery, OidcToken, OidcTokenResponse, OpenAiOAuthToken, OpenAiOAuthTokenResponse,
    PollOutcome, discover, poll_token_once, provider_error_body, request_device_code,
};
use anyhow::{Context, Result, anyhow, bail};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::cli::{AuthArgs, AuthCommand, AuthProviderChoice};
use crate::commands::path_util::display_path;
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
pub async fn run(config: &Config, args: AuthArgs, config_path: Option<&Path>) -> Result<()> {
    let i18n = &config.i18n;
    match args.command {
        AuthCommand::Login(args) => match args.provider {
            AuthProviderChoice::OpenaiOauth => {
                if args.github_token.is_some() || args.client_id.is_some() || args.issuer.is_some()
                {
                    bail!(
                        "{}", i18n.text("cli-auth-github-token-client-id-issuer-do-not-apply-to-auth-login-openai-oauth"));
                }
                login_openai_oauth(config, args.timeout_secs).await
            }
            AuthProviderChoice::Copilot => {
                if args.issuer.is_some() {
                    bail!(
                        "{}",
                        i18n.text("cli-auth-issuer-applies-only-to-auth-login-oidc-device")
                    );
                }
                login_copilot(config, args.timeout_secs, args.github_token, args.client_id).await
            }
            AuthProviderChoice::OidcDevice => {
                if args.github_token.is_some() {
                    bail!(
                        "{}",
                        i18n.text("cli-auth-github-token-applies-only-to-auth-login-copilot")
                    );
                }
                let issuer = args
                    .issuer
                    .context(i18n.text("cli-auth-issuer-is-required-for-auth-login-oidc-device"))?;
                let client_id = args.client_id.context(
                    i18n.text("cli-auth-client-id-is-required-for-auth-login-oidc-device"),
                )?;
                login_oidc_device(config, &issuer, &client_id, args.timeout_secs).await
            }
        },
        AuthCommand::Logout(args) => match args.provider {
            AuthProviderChoice::OpenaiOauth => logout_openai_oauth(config),
            AuthProviderChoice::Copilot => logout_copilot(config),
            AuthProviderChoice::OidcDevice => logout_oidc_device(config),
        },
        AuthCommand::Status(_) => status(config),
        AuthCommand::Enable(args) => super::auth_enable::run(config, &args, config_path),
    }
}

async fn login_oidc_device(
    config: &Config,
    issuer: &str,
    client_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    let i18n = &config.i18n;
    let client = auth_http_client()?;
    let discovery = discover(&client, issuer).await?;
    let device = request_device_code(&client, &discovery, client_id, OIDC_DEFAULT_SCOPE).await?;

    match device.verification_uri_complete.as_deref() {
        Some(complete) => println!(
            "{}",
            i18n.format(
                "cli-auth-open-this-url-complete",
                &[("value0", complete.to_string())]
            )
        ),
        None => {
            println!(
                "{}",
                i18n.format(
                    "cli-auth-open-this-url-complete",
                    &[("value0", device.verification_uri.to_string())]
                )
            );
            println!(
                "{}",
                i18n.format(
                    "cli-auth-enter-code",
                    &[("value0", device.user_code.to_string())]
                )
            );
        }
    }
    println!("{}", i18n.text("cli-auth-waiting-for-authorization"));

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

    println!("{}", i18n.text("cli-auth-oidc-device-logged-in"));
    println!(
        "{}",
        i18n.format("cli-auth-issuer-issuer", &[("value0", issuer.to_string())])
    );
    println!(
        "{}",
        i18n.format(
            "cli-auth-token-file",
            &[("value0", display_path(&path).to_string())]
        )
    );
    Ok(())
}

async fn poll_oidc_device(
    client: &reqwest::Client,
    discovery: &OidcDiscovery,
    client_id: &str,
    device: &DeviceAuthorizationResponse,
    timeout: Duration,
) -> Result<OidcTokenResponse> {
    let mut interval = Duration::from_secs(
        device
            .interval
            .unwrap_or(5)
            .max(1)
            .saturating_add(POLLING_SAFETY_MARGIN_SECS),
    );
    let device_timeout = Duration::from_secs(device.expires_in.unwrap_or(600).max(1));
    let timeout = timeout.min(device_timeout);
    with_authorization_timeout(timeout, "oidc-device", async {
        loop {
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
    })
    .await
}

/// Cancel the whole poll, including a response body or provider-directed sleep.
/// A ready deadline wins over a simultaneously ready token, so a late result
/// cannot reach the credential store. The interval between requests is unchanged.
async fn with_authorization_timeout<T>(
    timeout: Duration,
    provider: &str,
    authorization: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    if timeout.is_zero() {
        bail!("timed out waiting for {provider} authorization");
    }
    tokio::select! {
        biased;
        () = sleep(timeout) => bail!("timed out waiting for {provider} authorization"),
        result = authorization => result,
    }
}

fn logout_oidc_device(config: &Config) -> Result<()> {
    let i18n = &config.i18n;
    let path = config.oidc_device_token_path();
    OidcToken::remove(&path).map_err(anyhow::Error::from)?;
    println!("{}", i18n.text("cli-auth-oidc-device-logged-out"));
    println!(
        "{}",
        i18n.format(
            "cli-auth-token-file",
            &[("value0", display_path(&path).to_string())]
        )
    );
    Ok(())
}

async fn login_openai_oauth(config: &Config, timeout_secs: u64) -> Result<()> {
    let i18n = &config.i18n;
    let client = auth_http_client()?;
    let device = start_device_authorization(&client).await?;
    println!(
        "{}",
        i18n.format(
            "cli-auth-open-this-url-complete",
            &[("value0", DEVICE_BROWSER_URL.to_string())]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-auth-enter-code",
            &[("value0", device.user_code.to_string())]
        )
    );
    println!("{}", i18n.text("cli-auth-waiting-for-authorization"));

    let code =
        poll_device_authorization(&client, &device, Duration::from_secs(timeout_secs)).await?;
    let tokens = exchange_authorization_code(&client, code).await?;
    let refresh = tokens.refresh_token.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "{}",
            i18n.text("cli-auth-openai-oauth-token-response-did-not-include-refresh-token")
        )
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

    println!("{}", i18n.text("cli-auth-openai-oauth-logged-in"));
    if let Some(account_id) = token.extra.account_id.as_deref() {
        println!(
            "{}",
            i18n.format(
                "cli-auth-account-account-id",
                &[("value0", account_id.to_string())]
            )
        );
    }
    println!(
        "{}",
        i18n.format(
            "cli-auth-token-file",
            &[("value0", display_path(&path).to_string())]
        )
    );
    Ok(())
}

fn logout_openai_oauth(config: &Config) -> Result<()> {
    let i18n = &config.i18n;
    let path = config.openai_oauth_token_path();
    OpenAiOAuthToken::remove(&path).map_err(anyhow::Error::from)?;
    println!("{}", i18n.text("cli-auth-openai-oauth-logged-out"));
    println!(
        "{}",
        i18n.format(
            "cli-auth-token-file",
            &[("value0", display_path(&path).to_string())]
        )
    );
    Ok(())
}

async fn login_copilot(
    config: &Config,
    timeout_secs: u64,
    github_token_arg: Option<String>,
    client_id_arg: Option<String>,
) -> Result<()> {
    let i18n = &config.i18n;
    let github_token = match github_token_arg.filter(|s| !s.trim().is_empty()) {
        Some(token) => token,
        None => match config.copilot_github_token() {
            Some(token) => token.expose_secret().to_string(),
            None => {
                let client_id = client_id_arg
                    .as_deref()
                    .or_else(|| config.copilot_client_id())
                    .unwrap_or(GITHUB_COPILOT_CLIENT_ID);
                run_copilot_device_flow(i18n, client_id, timeout_secs).await?
            }
        },
    };

    let token = CopilotToken::from_github_token(github_token, None);
    let path = config.copilot_token_path();
    token.save(&path).map_err(anyhow::Error::from)?;
    println!("{}", i18n.text("cli-auth-copilot-logged-in"));
    println!(
        "{}",
        i18n.format(
            "cli-auth-token-file",
            &[("value0", display_path(&path).to_string())]
        )
    );
    Ok(())
}

fn logout_copilot(config: &Config) -> Result<()> {
    let i18n = &config.i18n;
    let path = config.copilot_token_path();
    CopilotToken::remove(&path).map_err(anyhow::Error::from)?;
    println!("{}", i18n.text("cli-auth-copilot-logged-out"));
    println!(
        "{}",
        i18n.format(
            "cli-auth-token-file",
            &[("value0", display_path(&path).to_string())]
        )
    );
    Ok(())
}

fn status(config: &Config) -> Result<()> {
    let i18n = &config.i18n;
    let openai_path = config.openai_oauth_token_path();
    match OpenAiOAuthToken::load(&openai_path).map_err(anyhow::Error::from)? {
        Some(token) => {
            println!("{}", i18n.text("cli-auth-openai-oauth-logged-in"));
            if let Some(account_id) = token.extra.account_id.as_deref() {
                println!(
                    "{}",
                    i18n.format(
                        "cli-auth-account-account-id",
                        &[("value0", account_id.to_string())]
                    )
                );
            }
            println!(
                "{}",
                i18n.format(
                    "cli-auth-expires-in",
                    &[(
                        "value0",
                        format_duration_until(token.expires_at_ms).to_string()
                    )]
                )
            );
            println!(
                "{}",
                i18n.format(
                    "cli-auth-token-file",
                    &[("value0", display_path(&openai_path).to_string())]
                )
            );
        }
        None => {
            println!("{}", i18n.text("cli-auth-openai-oauth-not-logged-in"));
            println!(
                "{}",
                i18n.format(
                    "cli-auth-token-file",
                    &[("value0", display_path(&openai_path).to_string())]
                )
            );
        }
    }

    let copilot_path = config.copilot_token_path();
    match CopilotToken::load(&copilot_path).map_err(anyhow::Error::from)? {
        Some(token) => {
            let has_refreshable_github = token.has_refreshable_github_token();
            let has_valid_cached = token.has_valid_cached_copilot_token();
            if has_refreshable_github {
                println!("{}", i18n.text("cli-auth-copilot-logged-in"));
            } else if has_valid_cached {
                println!(
                    "{}",
                    i18n.text(
                        "cli-auth-copilot-cached-token-valid-no-github-token-stored-for-refresh"
                    )
                );
            } else {
                println!(
                    "{}", i18n.text("cli-auth-copilot-not-logged-in-cached-token-expired-and-no-refreshable-github-token-s"));
            }
            if let Some(expires_at_ms) = token.github_expires_at_ms {
                println!(
                    "{}",
                    i18n.format(
                        "cli-auth-github-token-expires-in",
                        &[("value0", format_duration_until(expires_at_ms).to_string())]
                    )
                );
            }
            if let Some(expires_at_ms) = token.copilot_expires_at_ms {
                println!(
                    "{}",
                    i18n.format(
                        "cli-auth-cached-copilot-token-expires-in",
                        &[("value0", format_duration_until(expires_at_ms).to_string())]
                    )
                );
            }
            if let Some(api_base_url) = token.api_base_url.as_deref() {
                println!(
                    "{}",
                    i18n.format(
                        "cli-auth-api-base-api-base-url",
                        &[("value0", api_base_url.to_string())]
                    )
                );
            }
            println!(
                "{}",
                i18n.format(
                    "cli-auth-token-file",
                    &[("value0", display_path(&copilot_path).to_string())]
                )
            );
        }
        None => {
            println!("{}", i18n.text("cli-auth-copilot-not-logged-in"));
            println!(
                "{}",
                i18n.format(
                    "cli-auth-token-file",
                    &[("value0", display_path(&copilot_path).to_string())]
                )
            );
        }
    }

    let oidc_path = config.oidc_device_token_path();
    match OidcToken::load(&oidc_path).map_err(anyhow::Error::from)? {
        Some(token) => {
            println!("{}", i18n.text("cli-auth-oidc-device-logged-in"));
            println!(
                "{}",
                i18n.format(
                    "cli-auth-issuer-issuer",
                    &[("value0", token.extra.issuer.to_string())]
                )
            );
            println!(
                "{}",
                i18n.format(
                    "cli-auth-expires-in",
                    &[(
                        "value0",
                        format_duration_until(token.expires_at_ms).to_string()
                    )]
                )
            );
            println!(
                "{}",
                i18n.format(
                    "cli-auth-token-file",
                    &[("value0", display_path(&oidc_path).to_string())]
                )
            );
        }
        None => {
            println!("{}", i18n.text("cli-auth-oidc-device-not-logged-in"));
            println!(
                "{}",
                i18n.format(
                    "cli-auth-token-file",
                    &[("value0", display_path(&oidc_path).to_string())]
                )
            );
        }
    }
    Ok(())
}

async fn run_copilot_device_flow(
    i18n: &ai_memory_core::i18n::Localizer,
    client_id: &str,
    timeout_secs: u64,
) -> Result<String> {
    let client = auth_http_client()?;
    let device = start_github_device_authorization(&client, client_id).await?;
    println!(
        "{}",
        i18n.format(
            "cli-auth-open-this-url-complete",
            &[("value0", device.verification_uri.to_string())]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-auth-enter-code",
            &[("value0", device.user_code.to_string())]
        )
    );
    println!("{}", i18n.text("cli-auth-waiting-for-authorization"));
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
        // Authentication requests can carry device codes, authorization codes,
        // refresh tokens, or issued credentials. Do not let an IdP redirect
        // those request bodies to a different URL or a TLS downgrade.
        .redirect(reqwest::redirect::Policy::none())
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
        let body = provider_error_body(resp).await;
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
    let interval = Duration::from_secs(
        device
            .interval
            .parse::<u64>()
            .unwrap_or(5)
            .max(1)
            .saturating_add(POLLING_SAFETY_MARGIN_SECS),
    );
    with_authorization_timeout(timeout, "openai-oauth", async {
        loop {
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
                let body = provider_error_body(resp).await;
                bail!("openai-oauth authorization polling failed ({status}): {body}");
            }
            sleep(interval).await;
        }
    })
    .await
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
        let body = provider_error_body(resp).await;
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
        let body = provider_error_body(resp).await;
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
    let mut interval = Duration::from_secs(device.interval.unwrap_or(5).max(1));
    let device_timeout = Duration::from_secs(device.expires_in.max(1));
    let timeout = timeout.min(device_timeout);
    with_authorization_timeout(timeout, "copilot GitHub", async {
        loop {
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
                let body = provider_error_body(resp).await;
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
    })
    .await
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

    #[tokio::test(start_paused = true)]
    async fn authorization_timeout_covers_pending_work_and_keeps_provider_error_messages() {
        for provider in ["oidc-device", "openai-oauth", "copilot GitHub"] {
            let started = tokio::time::Instant::now();
            let result = with_authorization_timeout(Duration::from_secs(1), provider, async {
                sleep(Duration::from_secs(60)).await;
                Ok(())
            })
            .await;
            assert_eq!(
                result.unwrap_err().to_string(),
                format!("timed out waiting for {provider} authorization"),
            );
            assert_eq!(started.elapsed(), Duration::from_secs(1));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn authorization_timeout_wins_when_the_token_and_deadline_are_ready() {
        let result = with_authorization_timeout(Duration::from_secs(1), "oidc-device", async {
            sleep(Duration::from_secs(1)).await;
            Ok("late token")
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn authorization_timeout_zero_does_not_start_a_request() {
        let requested = std::cell::Cell::new(false);
        let result = with_authorization_timeout(Duration::ZERO, "oidc-device", async {
            requested.set(true);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert!(!requested.get());
    }

    #[tokio::test(start_paused = true)]
    async fn authorization_timeout_preserves_a_prompt_provider_error() {
        let started = tokio::time::Instant::now();
        let result: Result<()> =
            with_authorization_timeout(Duration::from_secs(60), "oidc-device", async {
                bail!("oidc-device authorization denied")
            })
            .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "oidc-device authorization denied"
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn auth_client_never_forwards_credentials_through_redirects() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_url = format!("http://{}/received", target.local_addr().unwrap());
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_url = format!("http://{}/token", source.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let response = auth_http_client()
            .unwrap()
            .post(source_url)
            .form(&[("device_code", "synthetic-secret")])
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err(),
            "an authentication request body reached the redirect target"
        );
    }
}
