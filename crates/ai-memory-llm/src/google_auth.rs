//! Google Application Default Credentials (ADC) for Vertex AI.
//!
//! Reads one of the two credential files ADC resolves to on a workstation or
//! server and trades it for a short-lived OAuth access token:
//!
//! * `authorized_user`: written by `gcloud auth application-default login`;
//!   refreshed with the OAuth `refresh_token` grant.
//! * `service_account`: a downloaded key; exchanged with a self-signed RS256
//!   JWT assertion (RFC 7523).
//!
//! Workload identity federation (`external_account`), impersonation and the
//! GCE metadata server are not supported and fail with a message naming the
//! file type. See
//! <https://cloud.google.com/docs/authentication/application-default-credentials>
//! and <https://developers.google.com/identity/protocols/oauth2/service-account#httprest>.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth_file::now_ms;
use crate::error::{LlmError, LlmResult};
use crate::response::{provider_error, response_json_limited};

/// OAuth scope Vertex AI accepts for `generateContent`.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Google's OAuth token endpoint, used when the credentials file names none.
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Refresh this long before Google's stated expiry, so a token never expires
/// between being handed out and reaching Vertex.
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

/// Lifetime requested for a service-account assertion; Google caps it at one
/// hour.
const ASSERTION_LIFETIME_SECS: u64 = 3600;

const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

const RELOGIN_HINT: &str = "run `gcloud auth application-default login` again or point \
                            GOOGLE_APPLICATION_CREDENTIALS at a valid service-account key";

/// Credentials parsed from an ADC JSON file.
enum AdcCredentials {
    ServiceAccount {
        client_email: String,
        key_pair: Box<RsaKeyPair>,
        token_uri: String,
        project_id: Option<String>,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: SecretString,
        refresh_token: SecretString,
        token_uri: String,
        quota_project_id: Option<String>,
    },
}

impl AdcCredentials {
    fn project(&self) -> Option<&str> {
        match self {
            Self::ServiceAccount { project_id, .. } => project_id.as_deref(),
            Self::AuthorizedUser {
                quota_project_id, ..
            } => quota_project_id.as_deref(),
        }
    }
}

#[derive(Deserialize)]
struct FileKind {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct ServiceAccountFile {
    client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Deserialize)]
struct AuthorizedUserFile {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    #[serde(default)]
    token_uri: Option<String>,
    #[serde(default)]
    quota_project_id: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

struct CachedToken {
    access_token: SecretString,
    expires_at_ms: u64,
}

struct Loaded {
    credentials: AdcCredentials,
    token: Option<CachedToken>,
}

/// Access-token source backed by an ADC credentials file.
///
/// The file is read on first use rather than at construction, so a server
/// configured for Vertex still starts when the file is not there yet, and
/// re-read after [`Self::invalidate`], so a fresh `gcloud` login is picked up
/// without a restart.
pub struct GoogleAdcTokenSource {
    client: reqwest::Client,
    credentials_file: PathBuf,
    state: Mutex<Option<Loaded>>,
}

impl std::fmt::Debug for GoogleAdcTokenSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleAdcTokenSource")
            .field("credentials_file", &self.credentials_file)
            .finish_non_exhaustive()
    }
}

impl GoogleAdcTokenSource {
    /// Build a token source for `credentials_file`.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(credentials_file: impl Into<PathBuf>) -> LlmResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(crate::redirect::same_origin_redirects())
            .build()?;
        Ok(Self {
            client,
            credentials_file: credentials_file.into(),
            state: Mutex::new(None),
        })
    }

    /// The credentials file this source reads.
    #[must_use]
    pub fn credentials_file(&self) -> &Path {
        &self.credentials_file
    }

    /// Project named by the credentials file: `project_id` for a service
    /// account, `quota_project_id` for a `gcloud` user login.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be read or parsed.
    pub async fn project(&self) -> LlmResult<Option<String>> {
        let mut state = self.state.lock().await;
        let loaded = self.load(&mut state)?;
        Ok(loaded.credentials.project().map(str::to_owned))
    }

    /// A valid access token, refreshed when the cached one is near expiry.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file is unusable or Google rejects
    /// the grant; a rate-limited or failing token endpoint stays a transient
    /// [`LlmError::Provider`].
    pub async fn access_token(&self) -> LlmResult<SecretString> {
        let mut state = self.state.lock().await;
        let loaded = self.load(&mut state)?;
        if let Some(token) = &loaded.token
            && now_ms().saturating_add(REFRESH_MARGIN_MS) < token.expires_at_ms
        {
            return Ok(token.access_token.clone());
        }
        let fresh = self.exchange(&loaded.credentials).await?;
        let access_token = fresh.access_token.clone();
        loaded.token = Some(fresh);
        Ok(access_token)
    }

    /// Drop the cached token and parsed file, after Vertex refused a token.
    pub async fn invalidate(&self) {
        *self.state.lock().await = None;
    }

    fn load<'a>(&self, state: &'a mut Option<Loaded>) -> LlmResult<&'a mut Loaded> {
        if state.is_none() {
            *state = Some(Loaded {
                credentials: read_credentials(&self.credentials_file)?,
                token: None,
            });
        }
        state
            .as_mut()
            .ok_or_else(|| LlmError::Auth("google credentials were not loaded".into()))
    }

    async fn exchange(&self, credentials: &AdcCredentials) -> LlmResult<CachedToken> {
        let issued_at_ms = now_ms();
        let (token_uri, form): (&str, Vec<(&str, String)>) = match credentials {
            AdcCredentials::ServiceAccount {
                client_email,
                key_pair,
                token_uri,
                ..
            } => {
                let assertion =
                    sign_assertion(key_pair, client_email, token_uri, issued_at_ms / 1000)?;
                (
                    token_uri,
                    vec![
                        ("grant_type", JWT_BEARER_GRANT.to_owned()),
                        ("assertion", assertion),
                    ],
                )
            }
            AdcCredentials::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
                ..
            } => (
                token_uri,
                vec![
                    ("grant_type", "refresh_token".to_owned()),
                    ("client_id", client_id.clone()),
                    ("client_secret", client_secret.expose_secret().to_owned()),
                    ("refresh_token", refresh_token.expose_secret().to_owned()),
                ],
            ),
        };
        let resp = self
            .client
            .post(token_uri)
            .timeout(Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS))
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let error = provider_error(resp).await;
            let LlmError::Provider {
                body, retry_after, ..
            } = error
            else {
                return Err(error);
            };
            // A throttled or failing token endpoint has not rejected the
            // credentials; keep it retryable and skip the re-login hint.
            if LlmError::status_is_transient(status.as_u16()) {
                return Err(LlmError::Provider {
                    status: status.as_u16(),
                    body: format!("google token exchange failed: {body}"),
                    retry_after,
                });
            }
            return Err(LlmError::Auth(format!(
                "google token exchange failed ({status}): {body}. {RELOGIN_HINT}"
            )));
        }
        let token: TokenResponse = response_json_limited(resp).await?;
        let lifetime_ms = token
            .expires_in
            .unwrap_or(ASSERTION_LIFETIME_SECS)
            .saturating_mul(1000);
        Ok(CachedToken {
            access_token: SecretString::from(token.access_token),
            expires_at_ms: issued_at_ms.saturating_add(lifetime_ms),
        })
    }
}

fn read_credentials(path: &Path) -> LlmResult<AdcCredentials> {
    let bytes = std::fs::read(path).map_err(|e| {
        LlmError::Auth(format!(
            "read google credentials {}: {e}; {RELOGIN_HINT}",
            path.display()
        ))
    })?;
    parse_credentials(&bytes).map_err(|reason| {
        LlmError::Auth(format!("google credentials {}: {reason}", path.display()))
    })
}

fn parse_credentials(bytes: &[u8]) -> Result<AdcCredentials, String> {
    let FileKind { kind } =
        serde_json::from_slice(bytes).map_err(|e| format!("not a credentials file: {e}"))?;
    match kind.as_str() {
        "service_account" => {
            let file: ServiceAccountFile =
                serde_json::from_slice(bytes).map_err(|e| format!("service account: {e}"))?;
            Ok(AdcCredentials::ServiceAccount {
                client_email: file.client_email,
                key_pair: Box::new(parse_private_key(&file.private_key)?),
                token_uri: file
                    .token_uri
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_owned()),
                project_id: file.project_id.filter(|p| !p.trim().is_empty()),
            })
        }
        "authorized_user" => {
            let file: AuthorizedUserFile =
                serde_json::from_slice(bytes).map_err(|e| format!("authorized user: {e}"))?;
            Ok(AdcCredentials::AuthorizedUser {
                client_id: file.client_id,
                client_secret: SecretString::from(file.client_secret),
                refresh_token: SecretString::from(file.refresh_token),
                token_uri: file
                    .token_uri
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_owned()),
                quota_project_id: file.quota_project_id.filter(|p| !p.trim().is_empty()),
            })
        }
        other => Err(format!(
            "credential type `{other}` is not supported; use `gcloud auth \
             application-default login` (authorized_user) or a service-account key"
        )),
    }
}

/// Parse the PKCS#8 PEM block Google puts in a service-account key's
/// `private_key`.
fn parse_private_key(pem: &str) -> Result<RsaKeyPair, String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|e| format!("private_key is not base64 PEM: {e}"))?;
    RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("private_key is not a PKCS#8 RSA key: {e}"))
}

fn sign_assertion(
    key_pair: &RsaKeyPair,
    client_email: &str,
    audience: &str,
    issued_at: u64,
) -> LlmResult<String> {
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": audience,
        "iat": issued_at,
        "exp": issued_at + ASSERTION_LIFETIME_SECS,
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut signature = vec![0_u8; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| LlmError::Auth("signing the service-account assertion failed".into()))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_credential_types_name_the_type() {
        let error = parse_credentials(br#"{"type":"external_account"}"#)
            .err()
            .expect("external_account is refused");
        assert!(error.contains("external_account"), "{error}");
    }

    #[test]
    fn a_file_without_a_type_is_refused() {
        assert!(parse_credentials(br#"{"client_id":"x"}"#).is_err());
    }

    #[test]
    fn authorized_user_defaults_the_token_endpoint_and_reads_the_quota_project() {
        let credentials = parse_credentials(
            br#"{"type":"authorized_user","client_id":"id","client_secret":"secret",
                "refresh_token":"refresh","quota_project_id":"my-project"}"#,
        )
        .expect("valid authorized_user file");
        assert_eq!(credentials.project(), Some("my-project"));
        let AdcCredentials::AuthorizedUser { token_uri, .. } = credentials else {
            panic!("expected authorized_user");
        };
        assert_eq!(token_uri, DEFAULT_TOKEN_URI);
    }

    #[test]
    fn a_blank_project_counts_as_none() {
        let credentials = parse_credentials(
            br#"{"type":"authorized_user","client_id":"id","client_secret":"secret",
                "refresh_token":"refresh","quota_project_id":"  "}"#,
        )
        .expect("valid authorized_user file");
        assert_eq!(credentials.project(), None);
    }

    #[test]
    fn a_service_account_key_that_is_not_rsa_pkcs8_is_refused() {
        let error = parse_credentials(
            br#"{"type":"service_account","client_email":"sa@p.iam.gserviceaccount.com",
                "private_key":"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n"}"#,
        )
        .err()
        .expect("garbage key is refused");
        assert!(error.contains("PKCS#8"), "{error}");
    }
}
