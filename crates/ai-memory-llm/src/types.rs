//! Provider-neutral request/response types.

use serde::{Deserialize, Serialize};

/// Message role in a chat turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Message from the user.
    User,
    /// Message from the assistant (the model).
    Assistant,
}

/// One message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role.
    pub role: Role,
    /// Message text.
    pub content: String,
}

impl ChatMessage {
    /// Convenience constructor.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Convenience constructor.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Provider-neutral chat completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Optional system prompt.
    pub system: Option<String>,
    /// User + assistant messages.
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature (0.0..2.0). `None` to defer to provider default.
    pub temperature: Option<f32>,
}

impl ChatRequest {
    /// Build a request from a single user prompt.
    #[must_use]
    pub fn user_prompt(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            messages: vec![ChatMessage::user(prompt)],
            max_tokens: 1024,
            temperature: None,
        }
    }

    /// Override the max-tokens cap.
    #[must_use]
    pub const fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

/// Token usage report. Providers return at least input/output counts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub input_tokens: u32,
    /// Tokens generated.
    pub output_tokens: u32,
}

/// Provider-neutral response.
#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    /// Model-assistant text output.
    pub text: String,
    /// Token usage, if reported.
    pub usage: Option<Usage>,
    /// Model identifier echoed by the provider.
    pub model: String,
}
