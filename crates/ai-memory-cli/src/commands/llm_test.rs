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
    };
    let client = build_provider(provider_config).context("building LLM provider")?;
    info!(
        provider = client.name(),
        model = client.model(),
        "sending prompt",
    );
    let resp = client
        .complete(ChatRequest::user_prompt(args.prompt))
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
}
