//! Provider authentication material.
//!
//! This module is the narrow boundary between configuration / login flows and
//! provider wire clients. API-key, OAuth-token-file, and token-exchange backed
//! providers extend this layer instead of teaching each provider to read env
//! vars directly.

use std::path::{Path, PathBuf};

use secrecy::SecretString;

use crate::error::{LlmError, LlmResult};

/// Credential source used for diagnostics and future `auth status` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Explicit CLI argument, such as `llm-test --api-key`.
    CliOverride,
    /// Process environment variable.
    Environment {
        /// Environment variable name.
        name: &'static str,
    },
    /// On-disk token file under ai-memory's data dir.
    TokenFile,
    /// No credential was supplied.
    NotProvided,
}

/// Auth requirement declared by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRequirement {
    /// Provider cannot be constructed without an API key.
    RequiredApiKey {
        /// Environment variable operators normally use for this API key.
        env_var: &'static str,
    },
    /// Provider accepts an API key but can also run without one.
    OptionalApiKey {
        /// Environment variable operators normally use for this API key.
        env_var: &'static str,
    },
    /// Provider requires a ChatGPT/Codex OAuth token file.
    OpenAiOAuthToken,
    /// Provider requires a GitHub token or stored auth for Copilot.
    CopilotToken,
    /// Provider requires an Anthropic OAuth subscription token
    /// (from `claude setup-token`).
    AnthropicOAuthToken,
}

/// Resolved Copilot auth inputs.
#[derive(Debug, Clone)]
pub struct CopilotAuth {
    /// Shared auth file under ai-memory's data dir.
    pub token_file: PathBuf,
    /// GitHub user token from env/config, if present.
    pub github_token: Option<SecretString>,
    /// Direct short-lived Copilot API token from env/config, if present.
    pub direct_api_token: Option<SecretString>,
    /// Optional Copilot API base URL override.
    pub api_base_url: Option<String>,
}

/// Materialized provider credential.
#[derive(Debug, Clone)]
pub enum Credential {
    /// Static API key / bearer secret.
    ApiKey(SecretString),
    /// Path to the OpenAI OAuth token file.
    OpenAiOAuthTokenFile(PathBuf),
    /// GitHub Copilot auth inputs.
    Copilot(CopilotAuth),
    /// Anthropic OAuth subscription token from `claude setup-token`.
    AnthropicOAuthToken(SecretString),
}

/// Resolved authentication for one provider instance.
#[derive(Debug, Clone)]
pub struct ProviderAuth {
    requirement: AuthRequirement,
    credential: Option<Credential>,
    source: CredentialSource,
}

impl ProviderAuth {
    /// Resolve a required API-key auth method from an environment value.
    #[must_use]
    pub fn required_api_key_from_env(env_var: &'static str, key: Option<SecretString>) -> Self {
        Self::from_api_key(
            AuthRequirement::RequiredApiKey { env_var },
            key,
            CredentialSource::Environment { name: env_var },
        )
    }

    /// Resolve an optional API-key auth method from an environment value.
    #[must_use]
    pub fn optional_api_key_from_env(env_var: &'static str, key: Option<SecretString>) -> Self {
        Self::from_api_key(
            AuthRequirement::OptionalApiKey { env_var },
            key,
            CredentialSource::Environment { name: env_var },
        )
    }

    /// Resolve OpenAI OAuth auth from a token file path.
    #[must_use]
    pub fn openai_oauth_token_file(path: impl Into<PathBuf>) -> Self {
        Self {
            requirement: AuthRequirement::OpenAiOAuthToken,
            credential: Some(Credential::OpenAiOAuthTokenFile(path.into())),
            source: CredentialSource::TokenFile,
        }
    }

    /// Resolve Copilot auth from a shared token file plus optional env tokens.
    #[must_use]
    pub fn copilot(
        token_file: impl Into<PathBuf>,
        github_token: Option<SecretString>,
        direct_api_token: Option<SecretString>,
        api_base_url: Option<String>,
    ) -> Self {
        Self {
            requirement: AuthRequirement::CopilotToken,
            credential: Some(Credential::Copilot(CopilotAuth {
                token_file: token_file.into(),
                github_token,
                direct_api_token,
                api_base_url,
            })),
            source: CredentialSource::TokenFile,
        }
    }

    /// Resolve Anthropic OAuth auth from a subscription token (from `claude setup-token`).
    #[must_use]
    pub fn anthropic_oauth_token(token: Option<SecretString>) -> Self {
        let has_token = token.is_some();
        Self {
            requirement: AuthRequirement::AnthropicOAuthToken,
            credential: token.map(Credential::AnthropicOAuthToken),
            source: if has_token {
                CredentialSource::Environment {
                    name: "ANTHROPIC_OAUTH_TOKEN",
                }
            } else {
                CredentialSource::NotProvided
            },
        }
    }

    fn from_api_key(
        requirement: AuthRequirement,
        key: Option<SecretString>,
        source_if_present: CredentialSource,
    ) -> Self {
        let has_key = key.is_some();
        Self {
            requirement,
            credential: key.map(Credential::ApiKey),
            source: if has_key {
                source_if_present
            } else {
                CredentialSource::NotProvided
            },
        }
    }

    /// Override the resolved credential with a CLI-provided API key.
    #[must_use]
    pub fn with_cli_api_key_override(mut self, key: Option<SecretString>) -> Self {
        if let Some(key) = key {
            self.credential = Some(Credential::ApiKey(key));
            self.source = CredentialSource::CliOverride;
        }
        self
    }

    /// Return the declared auth requirement.
    #[must_use]
    pub const fn requirement(&self) -> AuthRequirement {
        self.requirement
    }

    /// Return where the credential came from, if any.
    #[must_use]
    pub const fn source(&self) -> CredentialSource {
        self.source
    }

    /// Extract a required API key, preserving today's missing-env error shape.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] with the required env var name when
    /// the provider requires an API key and none was resolved.
    pub fn require_api_key(&self) -> LlmResult<SecretString> {
        match (&self.requirement, &self.credential) {
            (_, Some(Credential::ApiKey(key))) => Ok(key.clone()),
            (_, Some(Credential::OpenAiOAuthTokenFile(_))) => Err(LlmError::NotConfigured(
                "API key credential expected, got openai-oauth token file".into(),
            )),
            (_, Some(Credential::Copilot(_))) => Err(LlmError::NotConfigured(
                "API key credential expected, got copilot auth".into(),
            )),
            (_, Some(Credential::AnthropicOAuthToken(_))) => Err(LlmError::NotConfigured(
                "API key credential expected, got anthropic-oauth token".into(),
            )),
            (AuthRequirement::RequiredApiKey { env_var }, None) => {
                Err(LlmError::NotConfigured((*env_var).into()))
            }
            (AuthRequirement::OptionalApiKey { env_var }, None) => {
                Err(LlmError::NotConfigured((*env_var).into()))
            }
            (AuthRequirement::OpenAiOAuthToken, None) => Err(LlmError::NotConfigured(
                "openai-oauth token file missing; run `ai-memory auth login openai-oauth`".into(),
            )),
            (AuthRequirement::CopilotToken, None) => Err(LlmError::NotConfigured(
                "copilot auth missing; run `ai-memory auth login copilot` or set COPILOT_GITHUB_TOKEN"
                    .into(),
            )),
            (AuthRequirement::AnthropicOAuthToken, None) => Err(LlmError::NotConfigured(
                "anthropic-oauth token missing; run `claude setup-token` and set \
                 ANTHROPIC_OAUTH_TOKEN (or CLAUDE_CODE_OAUTH_TOKEN)"
                    .into(),
            )),
        }
    }

    /// Extract an optional API key.
    #[must_use]
    pub fn optional_api_key(&self) -> Option<SecretString> {
        match &self.credential {
            Some(Credential::ApiKey(key)) => Some(key.clone()),
            Some(
                Credential::OpenAiOAuthTokenFile(_)
                | Credential::Copilot(_)
                | Credential::AnthropicOAuthToken(_),
            )
            | None => None,
        }
    }

    /// Extract the Anthropic OAuth subscription token.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] when no token was resolved.
    pub fn require_anthropic_oauth_token(&self) -> LlmResult<SecretString> {
        match (&self.requirement, &self.credential) {
            (
                AuthRequirement::AnthropicOAuthToken,
                Some(Credential::AnthropicOAuthToken(token)),
            ) => Ok(token.clone()),
            (AuthRequirement::AnthropicOAuthToken, None) => Err(LlmError::NotConfigured(
                "anthropic-oauth token missing; run `claude setup-token` and set \
                 ANTHROPIC_OAUTH_TOKEN (or CLAUDE_CODE_OAUTH_TOKEN)"
                    .into(),
            )),
            _ => Err(LlmError::NotConfigured(
                "anthropic-oauth token credential required".into(),
            )),
        }
    }

    /// Extract the OpenAI OAuth token file path.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] if this auth material is not an
    /// OpenAI OAuth token-file credential.
    pub fn require_openai_oauth_token_file(&self) -> LlmResult<&Path> {
        match (&self.requirement, &self.credential) {
            (AuthRequirement::OpenAiOAuthToken, Some(Credential::OpenAiOAuthTokenFile(path))) => {
                Ok(path)
            }
            (AuthRequirement::OpenAiOAuthToken, None) => Err(LlmError::NotConfigured(
                "openai-oauth token file missing; run `ai-memory auth login openai-oauth`".into(),
            )),
            _ => Err(LlmError::NotConfigured(
                "openai-oauth token file credential required".into(),
            )),
        }
    }

    /// Extract Copilot auth inputs.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] if this auth material is not a
    /// Copilot credential.
    pub fn require_copilot_auth(&self) -> LlmResult<CopilotAuth> {
        match (&self.requirement, &self.credential) {
            (AuthRequirement::CopilotToken, Some(Credential::Copilot(auth))) => Ok(auth.clone()),
            (AuthRequirement::CopilotToken, None) => Err(LlmError::NotConfigured(
                "copilot auth missing; run `ai-memory auth login copilot` or set COPILOT_GITHUB_TOKEN"
                    .into(),
            )),
            _ => Err(LlmError::NotConfigured(
                "copilot auth credential required".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;

    use super::*;

    #[test]
    fn required_api_key_reports_env_name_when_missing() {
        let auth = ProviderAuth::required_api_key_from_env("OPENAI_API_KEY", None);
        let err = auth.require_api_key().unwrap_err();
        assert!(matches!(err, LlmError::NotConfigured(msg) if msg == "OPENAI_API_KEY"));
        assert_eq!(auth.source(), CredentialSource::NotProvided);
    }

    #[test]
    fn required_api_key_returns_secret_when_present() {
        let auth = ProviderAuth::required_api_key_from_env(
            "OPENAI_API_KEY",
            Some(SecretString::from("sk-test")),
        );
        assert_eq!(auth.require_api_key().unwrap().expose_secret(), "sk-test");
        assert_eq!(
            auth.source(),
            CredentialSource::Environment {
                name: "OPENAI_API_KEY"
            }
        );
    }

    #[test]
    fn cli_override_takes_precedence() {
        let auth = ProviderAuth::required_api_key_from_env(
            "OPENAI_API_KEY",
            Some(SecretString::from("env-key")),
        )
        .with_cli_api_key_override(Some(SecretString::from("cli-key")));

        assert_eq!(auth.require_api_key().unwrap().expose_secret(), "cli-key");
        assert_eq!(auth.source(), CredentialSource::CliOverride);
    }

    #[test]
    fn optional_api_key_allows_absence() {
        let auth = ProviderAuth::optional_api_key_from_env("LLM_API_KEY", None);
        assert!(auth.optional_api_key().is_none());
        assert_eq!(auth.source(), CredentialSource::NotProvided);
    }

    #[test]
    fn openai_oauth_token_file_round_trips_path() {
        let auth = ProviderAuth::openai_oauth_token_file("/tmp/oauth_token.json");
        assert_eq!(auth.source(), CredentialSource::TokenFile);
        assert_eq!(
            auth.require_openai_oauth_token_file().unwrap(),
            Path::new("/tmp/oauth_token.json")
        );
    }

    #[test]
    fn copilot_auth_round_trips_inputs() {
        let auth = ProviderAuth::copilot(
            "/tmp/auth.json",
            Some(SecretString::from("ghu-test")),
            None,
            Some("https://api.example.test".into()),
        );
        let copilot = auth.require_copilot_auth().unwrap();
        assert_eq!(copilot.token_file, Path::new("/tmp/auth.json"));
        assert_eq!(copilot.github_token.unwrap().expose_secret(), "ghu-test");
        assert_eq!(
            copilot.api_base_url.as_deref(),
            Some("https://api.example.test")
        );
    }

    #[test]
    fn anthropic_oauth_token_round_trips_secret_and_reports_env_source() {
        let auth = ProviderAuth::anthropic_oauth_token(Some(SecretString::from("oauth-tok-test")));
        assert_eq!(
            auth.source(),
            CredentialSource::Environment {
                name: "ANTHROPIC_OAUTH_TOKEN"
            }
        );
        assert_eq!(
            auth.require_anthropic_oauth_token()
                .unwrap()
                .expose_secret(),
            "oauth-tok-test"
        );
        assert_eq!(auth.requirement(), AuthRequirement::AnthropicOAuthToken);
    }

    #[test]
    fn anthropic_oauth_token_absent_returns_not_configured() {
        let auth = ProviderAuth::anthropic_oauth_token(None);
        assert_eq!(auth.source(), CredentialSource::NotProvided);
        let err = auth.require_anthropic_oauth_token().unwrap_err();
        assert!(
            matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("ANTHROPIC_OAUTH_TOKEN")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn require_api_key_rejects_anthropic_oauth_credential() {
        let auth = ProviderAuth::anthropic_oauth_token(Some(SecretString::from("tok")));
        let err = auth.require_api_key().unwrap_err();
        assert!(
            matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("anthropic-oauth token")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn optional_api_key_returns_none_for_anthropic_oauth_credential() {
        let auth = ProviderAuth::anthropic_oauth_token(Some(SecretString::from("tok")));
        assert!(auth.optional_api_key().is_none());
    }
}
