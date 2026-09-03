//! `ai-memory llm-test` — smoke test an LLM provider end-to-end.

use ai_memory_llm::{ChatRequest, ProviderChoice, ProviderConfig, build_provider};
use anyhow::{Context, Result};
use tracing::info;

use crate::cli::{LlmProviderChoice, LlmTestArgs};
use crate::config::Config;

/// Run the `llm-test` subcommand.
///
/// # Errors
/// Returns an error if the provider cannot be constructed, the env
/// lacks the required keys, or the HTTP call fails.
pub async fn run(config: &Config, args: LlmTestArgs) -> Result<()> {
    let provider = ProviderChoice::from(args.provider);
    let api_key_override = args
        .api_key
        .filter(|s| !s.is_empty())
        .map(secrecy::SecretString::from);
    let provider_config = ProviderConfig {
        provider,
        model: args.model,
        auth: config.provider_auth(provider, api_key_override),
        base_url: args.base_url.or_else(|| config.llm_test_base_url()),
        compat_strict: config.llm_compat_strict,
        request_timeout_secs: config.llm_timeout_secs,
        reasoning_effort: config.llm_reasoning_effort,
        extra_headers: config
            .llm_extra_headers()
            .context("parsing AI_MEMORY_LLM_HEADERS / llm_headers")?,
    };
    let client = build_provider(provider_config).context("building LLM provider")?;
    info!(
        provider = client.name(),
        model = client.model(),
        "sending prompt",
    );
    let resp = client
        .complete(representative_request(args.prompt))
        .await
        .context("calling provider")?;

    println!("--- model: {} ---", resp.model);
    if let Some(u) = resp.usage {
        println!(
            "--- usage: in={} out={} ---",
            u.input_tokens, u.output_tokens
        );
    }
    println!("{}", resp.text);
    Ok(())
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
            LlmProviderChoice::Copilot => Self::Copilot,
            LlmProviderChoice::Opencode => Self::OpenCode,
        }
    }
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
    fn llm_test_exercises_pipeline_sampling_compatibility() {
        let request = representative_request("diagnostic".into());

        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(request.messages[0].content, "diagnostic");
    }
}
