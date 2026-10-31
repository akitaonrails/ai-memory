//! `ai-memory llm-test` — smoke test an LLM provider end-to-end.

use ai_memory_llm::{ChatRequest, LlmError, ProviderChoice, ProviderConfig, build_provider};
use anyhow::{Context, Result};
use tracing::info;

use crate::cli::{LlmProviderChoice, LlmTestArgs};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Run the `llm-test` subcommand.
///
/// # Errors
/// Returns an error if the provider cannot be constructed, the env
/// lacks the required keys, or the HTTP call fails.
pub async fn run(config: &Config, args: LlmTestArgs) -> Result<()> {
    let i18n = &config.i18n;
    if args.server {
        return run_against_server(config).await;
    }
    let provider = ProviderChoice::from(
        args.provider
            .context(i18n.text("cli-llm-test-provider-is-required-unless-server-is-passed"))?,
    );
    let api_key_override = args
        .api_key
        .filter(|s| !s.is_empty())
        .map(secrecy::SecretString::from);
    let provider_config = ProviderConfig {
        provider,
        model: args
            .model
            .context(i18n.text("cli-llm-test-model-is-required-unless-server-is-passed"))?,
        auth: config.provider_auth(provider, api_key_override),
        base_url: args.base_url.or_else(|| config.llm_test_base_url(provider)),
        compat_strict: config.llm_compat_strict,
        compat_disable_thinking: config.llm_compat_disable_thinking,
        compat_stream: config.llm_compat_stream,
        request_timeout_secs: config.llm_timeout_secs,
        reasoning_effort: config.llm_reasoning_effort,
        extra_headers: config
            .llm_extra_headers()
            .context(i18n.text("cli-llm-test-parsing-ai-memory-llm-headers-llm-headers"))?,
    };
    let client =
        build_provider(provider_config).context(i18n.text("cli-llm-test-building-llm-provider"))?;
    info!(
        provider = client.name(),
        model = client.model(),
        "sending prompt",
    );
    let request = representative_request(
        args.prompt
            .context(i18n.text("cli-llm-test-prompt-is-required-unless-server-is-passed"))?,
    );
    if args.structured {
        let value = client
            .complete_structured_raw(
                request,
                serde_json::json!({
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"]
                }),
            )
            .await
            .map_err(|e| with_oauth_hint(provider, e))
            .context(i18n.text("cli-llm-test-calling-provider-for-structured-output"))?;
        println!(
            "{}",
            i18n.format(
                "cli-llm-test-model",
                &[("value0", client.model().to_string())]
            )
        );
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let resp = client
        .complete(request)
        .await
        .map_err(|e| with_oauth_hint(provider, e))
        .context(i18n.text("cli-llm-test-calling-provider"))?;

    println!(
        "{}",
        i18n.format("cli-llm-test-model", &[("value0", resp.model.to_string())])
    );
    if let Some(u) = resp.usage {
        println!(
            "{}",
            i18n.format(
                "cli-llm-test-usage-in-out",
                &[
                    ("value0", format!("{}", u.input_tokens)),
                    ("value1", format!("{}", u.output_tokens))
                ]
            )
        );
    }
    println!("{}", resp.text);
    Ok(())
}

/// Anthropic answers a subscription token it will not serve with a bare 429
/// that reads like a real rate limit (#1170).
const ANTHROPIC_OAUTH_429_HINT: &str = "Anthropic often answers `claude setup-token` \
tokens with this 429 for any client that is not Claude Code; it is not a real \
rate limit. Use `--provider anthropic` with an API key or an OpenAI-compatible \
gateway (see docs/llm-providers.md).";

fn with_oauth_hint(provider: ProviderChoice, err: LlmError) -> anyhow::Error {
    let is_oauth_429 = provider == ProviderChoice::AnthropicOAuth
        && matches!(err, LlmError::Provider { status: 429, .. });
    let err = anyhow::Error::new(err);
    if is_oauth_429 {
        err.context(ANTHROPIC_OAUTH_429_HINT)
    } else {
        err
    }
}

/// Use the same sampling value as bootstrap and consolidation so this smoke
/// test exercises provider-specific request normalization.
fn representative_request(prompt: String) -> ChatRequest {
    let mut request = ChatRequest::user_prompt(prompt);
    request.temperature = Some(0.2);
    request
}

impl From<LlmProviderChoice> for ProviderChoice {
    fn from(value: LlmProviderChoice) -> Self {
        match value {
            LlmProviderChoice::Anthropic => Self::Anthropic,
            LlmProviderChoice::AnthropicOauth => Self::AnthropicOAuth,
            LlmProviderChoice::Openai => Self::OpenAi,
            LlmProviderChoice::Gemini => Self::Gemini,
            LlmProviderChoice::OpenaiCompat => Self::OpenAiCompat,
            LlmProviderChoice::OpenaiOauth => Self::OpenAiOAuth,
            LlmProviderChoice::Codex => Self::Codex,
            LlmProviderChoice::Copilot => Self::Copilot,
            LlmProviderChoice::Opencode => Self::OpenCode,
            LlmProviderChoice::Cursor => Self::Cursor,
        }
    }
}

/// Probe the server's own configured provider (`--server`).
///
/// The server runs the probe with a fixed short prompt against what its
/// configuration holds, so this works from a machine that has no keys at all —
/// which is the whole point (#1639).
async fn run_against_server(config: &Config) -> Result<()> {
    let i18n = &config.i18n;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(&endpoint, "/admin/llm-test", &()).await?;
    let provider = report["provider"].as_str().unwrap_or("(unknown)");
    let model = report["model"].as_str().unwrap_or("(unknown)");
    let latency = report["latency_ms"].as_u64().unwrap_or(0);
    println!(
        "{}",
        i18n.format(
            "cli-llm-test-server-provider-provider-model-latency-ms",
            &[
                ("value0", provider.to_string()),
                ("value1", model.to_string()),
                ("value2", format!("{}", latency))
            ]
        )
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_oauth_choice_maps_to_runtime_provider() {
        assert_eq!(
            ProviderChoice::from(LlmProviderChoice::AnthropicOauth),
            ProviderChoice::AnthropicOAuth
        );
    }

    #[test]
    fn cursor_choice_maps_to_runtime_provider() {
        assert_eq!(
            ProviderChoice::from(LlmProviderChoice::Cursor),
            ProviderChoice::Cursor
        );
    }

    #[test]
    fn codex_choice_maps_to_runtime_provider() {
        assert_eq!(
            ProviderChoice::from(LlmProviderChoice::Codex),
            ProviderChoice::Codex
        );
    }

    #[test]
    fn anthropic_oauth_429_carries_the_hint() {
        let err = with_oauth_hint(
            ProviderChoice::AnthropicOAuth,
            LlmError::Provider {
                status: 429,
                body: "rate_limit_error".into(),
                retry_after: None,
            },
        );
        assert!(format!("{err:#}").contains("not a real rate limit"));
    }

    #[test]
    fn other_providers_and_statuses_carry_no_hint() {
        for (provider, status) in [
            (ProviderChoice::Anthropic, 429),
            (ProviderChoice::AnthropicOAuth, 500),
        ] {
            let err = with_oauth_hint(
                provider,
                LlmError::Provider {
                    status,
                    body: String::new(),
                    retry_after: None,
                },
            );
            assert!(!format!("{err:#}").contains("not a real rate limit"));
        }
    }

    #[test]
    fn llm_test_exercises_pipeline_sampling_compatibility() {
        let request = representative_request("diagnostic".into());

        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(request.messages[0].content, "diagnostic");
    }
}
