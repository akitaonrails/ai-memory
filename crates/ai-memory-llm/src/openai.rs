//! OpenAI Chat Completions client (with `response_format` JSON schema for
//! structured output).

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::text::truncate_with_ellipsis;
use crate::types::{ChatRequest, ChatResponse, Role, Usage};

/// Default OpenAI API base.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Build the full URL for an OpenAI-style endpoint. Tolerates the
/// conventions found in the wild:
///   * `https://api.openai.com`           (OpenAI's own docs)
///   * `https://openrouter.ai/api/v1`     (OpenRouter's docs)
///   * `http://localhost:11434/v1`        (Ollama's openai-compat path)
///   * `https://api.z.ai/api/coding/paas/v4` (Z.AI)
///
/// Without this, half the providers produce `…/v1/v1/…` 404s the
/// first time consolidation runs.
#[must_use]
pub fn normalize_openai_base(base: &str, endpoint: &str) -> String {
    let s = base.trim_end_matches('/');

    if s.ends_with(&format!("/{endpoint}")) {
        return s.to_string();
    }

    if last_segment_is_version(s) {
        return format!("{s}/{endpoint}");
    }

    format!("{s}/v1/{endpoint}")
}

fn last_segment_is_version(url: &str) -> bool {
    url.split('/').next_back().is_some_and(|seg| {
        let digits = seg.strip_prefix('v').unwrap_or("");
        !digits.is_empty() && digits.len() <= 2 && digits.chars().all(|c| c.is_ascii_digit())
    })
}

/// Request dialect — picks which OpenAI quirks the provider applies.
///
/// `Official` targets `api.openai.com` and honours the model-family
/// rules that the real OpenAI Chat Completions endpoint enforces:
/// `max_completion_tokens` for gpt-5 / o-series, model-family output
/// caps, omitted `temperature` for reasoning models, strict-mode JSON
/// schema normalisation.
///
/// `Compat` targets the OpenAI-compatible wire format spoken by
/// Ollama, vLLM, LM Studio, llama.cpp, and the long tail of local /
/// proxy backends. Those backends almost universally implement the
/// legacy `max_tokens` dialect, ignore OpenAI-specific output caps,
/// and accept any temperature value — so we keep the request shape
/// stable and let the engine clamp / coerce as it sees fit. Forcing
/// the official dialect onto compat backends would break working
/// Ollama / vLLM setups (issue raised in PR review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDialect {
    /// Official `api.openai.com`. Apply per-model quirks.
    Official,
    /// Local / proxy `openai-compat` (Ollama, vLLM, LM Studio, …).
    /// Legacy `max_tokens` only, no caps, no temperature massaging.
    Compat,
}

/// OpenAI Chat Completions-backed provider.
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    dialect: RequestDialect,
}

impl OpenAiProvider {
    /// Construct a provider given an API key + model id. Defaults to
    /// the `Official` dialect (targeting `api.openai.com`). Override
    /// with [`with_dialect`] when wrapping for `openai-compat`.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        // 300s tolerates Ollama / llama-swap cold-loading a 30B+ model
        // from disk on first request. Once OLLAMA_KEEP_ALIVE keeps it
        // warm, subsequent requests return in seconds — but the first
        // one after the model unloaded needs the headroom.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            dialect: RequestDialect::Official,
        })
    }

    /// Override the API base URL (tests; or pointing at an
    /// OpenAI-compatible mirror).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Switch request dialect. See [`RequestDialect`].
    #[must_use]
    pub fn with_dialect(mut self, dialect: RequestDialect) -> Self {
        self.dialect = dialect;
        self
    }
}

#[derive(Debug, Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMsg<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAiResponseFormat>,
}

#[derive(Debug, Serialize)]
struct OpenAiMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiResponseFormat {
    JsonSchema { json_schema: OpenAiJsonSchema },
}

#[derive(Debug, Serialize)]
struct OpenAiJsonSchema {
    name: String,
    schema: serde_json::Value,
    strict: bool,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    model: String,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessageResponse,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessageResponse {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let response = self.post(&self.build_request(&request, None)).await?;
        Ok(self.to_chat_response(response))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        // Strict-mode normalisation is an `Official` concern — compat
        // backends typically ignore `response_format` entirely and fall
        // back to "parse the first JSON object out of the text".
        if self.dialect == RequestDialect::Official {
            enforce_strict_object_schemas(&mut schema);
        }
        let response_format = OpenAiResponseFormat::JsonSchema {
            json_schema: OpenAiJsonSchema {
                name: "Result".into(),
                schema,
                strict: true,
            },
        };
        let response = self
            .post(&self.build_request(&request, Some(response_format)))
            .await?;
        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("");
        serde_json::from_str::<serde_json::Value>(text).map_err(LlmError::from)
    }
}

impl OpenAiProvider {
    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        response_format: Option<OpenAiResponseFormat>,
    ) -> OpenAiRequest<'a> {
        let mut messages: Vec<OpenAiMsg<'a>> = Vec::new();
        if let Some(sys) = request.system.as_deref() {
            messages.push(OpenAiMsg {
                role: "system",
                content: sys,
            });
        }
        for m in &request.messages {
            messages.push(OpenAiMsg {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            });
        }
        // `Compat` backends (Ollama, vLLM, LM Studio, …) speak the
        // legacy OpenAI wire format only: always `max_tokens`, never
        // OpenAI-side caps, never temperature-omission. The engine
        // itself clamps oversized requests; forcing the official
        // dialect onto them is the regression Akita flagged in review.
        let (max_tokens, max_completion_tokens, temperature) = match self.dialect {
            RequestDialect::Compat => (Some(request.max_tokens), None, request.temperature),
            RequestDialect::Official => {
                let capped = request.max_tokens.min(max_output_tokens_for(&self.model));
                let (mt, mct) = if model_requires_max_completion_tokens(&self.model) {
                    (None, Some(capped))
                } else {
                    (Some(capped), None)
                };
                // gpt-5 and o-series reject any non-default temperature
                // with `Unsupported value: temperature does not support
                // 0.2 with this model. Only the default (1) is
                // supported.` The lint / consolidate / bootstrap call
                // sites all pass 0.1-0.2; omit the field entirely so
                // the API uses its model-specific default.
                let temp = if model_requires_default_temperature(&self.model) {
                    None
                } else {
                    request.temperature
                };
                (mt, mct, temp)
            }
        };
        OpenAiRequest {
            model: &self.model,
            messages,
            max_tokens,
            max_completion_tokens,
            temperature,
            response_format,
        }
    }

    fn to_chat_response(&self, response: OpenAiResponse) -> ChatResponse {
        let text = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        ChatResponse {
            text,
            usage: response.usage.map(|u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            }),
            model: response.model,
        }
    }

    async fn post<B: Serialize>(&self, body: &B) -> LlmResult<OpenAiResponse> {
        let url = normalize_openai_base(&self.base_url, "chat/completions");
        debug!(url, "POST openai");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.api_key.expose_secret())
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: truncate_with_ellipsis(&body, 1024),
            });
        }
        resp.json::<OpenAiResponse>().await.map_err(LlmError::from)
    }
}

/// Recursively normalise a JSON schema for OpenAI Structured Outputs
/// (`strict: true`). The endpoint rejects schemas missing either:
///
/// 1. `additionalProperties: false` on every object node — without it:
///    `'additionalProperties' is required to be supplied and to be false`.
///
/// 2. `required` listing **every** key in `properties` (strict mode does
///    not support optional fields; callers that need optionality express
///    it via a nullable type instead, e.g. `["string", "null"]`). Without
///    a complete `required` array: `'required' is required to be supplied
///    and to be an array including every key in properties`.
///
/// Both rules are unconditional here: this normalisation only runs on
/// the `Official` request dialect, which targets `api.openai.com`
/// where strict mode is mandatory. Any caller-supplied
/// `additionalProperties: true` or trimmed `required` array is
/// overwritten — preserving them would let invalid schemas through
/// and re-introduce the 400 this function exists to prevent. Callers
/// that need looser schemas should use the `Compat` dialect (which
/// skips this normalisation entirely) or a non-strict path.
pub(crate) fn enforce_strict_object_schemas(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // OpenAI's structured-output subset rejects any sibling
            // keyword next to `$ref` with a 400. schemars 1.x emits a
            // field-level `description` next to `$ref` for doc-commented
            // fields typed as external enums (e.g. `tier: Tier` on
            // `ConsolidatedPageUpdate`); without this strip,
            // `memory_consolidate multi_page=true` fails before the model
            // runs. In our generated schemas those siblings are
            // annotations, not validation constraints, so the referenced
            // definition remains the source of truth.
            if map.contains_key("$ref") {
                map.retain(|k, _| k == "$ref");
                return;
            }
            // OpenAI strict mode rejects `oneOf` outright but accepts
            // `anyOf`. schemars 1.x emits `oneOf` for closed Rust enums
            // such as `Tier` and `PageKind` under `$defs` in
            // `ConsolidatedBatch`; their const branches are disjoint, so
            // the rewrite preserves the generated schema's accepted set.
            if let Some(one_of) = map.remove("oneOf") {
                map.insert("anyOf".to_string(), one_of);
            }
            let is_object = map
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "object")
                || map.contains_key("properties");
            if is_object {
                // Force-set both: a caller-supplied `true` would defeat
                // the entire purpose of the strict-mode normalisation.
                map.insert("additionalProperties".to_string(), serde_json::json!(false));
                // OpenAI strict mode rejects ANY incomplete `required` —
                // even an explicit subset. The only way to express
                // optionality is via a nullable type at the value site
                // (e.g. `["string", "null"]`). Overwrite unconditionally
                // when `properties` is present so a caller-supplied
                // partial list doesn't sneak through.
                if let Some(props) = map.get("properties").and_then(|p| p.as_object()) {
                    let keys: Vec<serde_json::Value> =
                        props.keys().map(|k| serde_json::json!(k)).collect();
                    map.insert("required".to_string(), serde_json::Value::Array(keys));
                }
            }
            for (_, v) in map.iter_mut() {
                enforce_strict_object_schemas(v);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                enforce_strict_object_schemas(v);
            }
        }
        _ => {}
    }
}

/// Models that require `max_completion_tokens` instead of `max_tokens`.
/// OpenAI introduced this rename starting with the reasoning-capable o1
/// family and made it mandatory across the gpt-5 line. Sending the legacy
/// `max_tokens` to these models returns a 400 with
/// `Unsupported parameter: 'max_tokens'`.
pub(crate) fn model_requires_max_completion_tokens(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

/// Models that reject any non-default `temperature` value.
///
/// gpt-5 and the o-series reasoning models accept only the model
/// default (1.0). Any caller-supplied value — including the 0.1-0.2
/// passed by lint / bootstrap / consolidation — returns a 400:
/// `Unsupported value: 'temperature' does not support 0.2 with this
/// model. Only the default (1) is supported.` Omitting the field
/// entirely lets the API apply its own default and unblocks those
/// models without forcing every call site to be model-aware.
pub(crate) fn model_requires_default_temperature(model: &str) -> bool {
    // Same family as `max_completion_tokens` — keep aligned: any future
    // family that adopts the new rename also tends to lock temperature.
    model_requires_max_completion_tokens(model)
}

/// Per-model output-token ceiling for the `Official` dialect.
///
/// OpenAI rejects requests above the model's published limit with
/// `400 max_tokens is too large`, instead of silently truncating.
/// Callers (e.g. bootstrap) deliberately ask for very large budgets
/// (64K) so Anthropic / Haiku-class models don't truncate mid-JSON;
/// the same request blows up on gpt-4o-mini without this defensive
/// cap. The cap is informed but conservative: gpt-4-turbo's real
/// limit is 4096 (smaller than what we use here), so a max-budget
/// bootstrap call to gpt-4-turbo will still 400 with the same
/// model-specific message — at which point the operator can lower
/// `max_tokens` or switch model. The cap exists to unblock the
/// common case (gpt-4o family at 16384), not to paper over every
/// model. Reasoning models in the gpt-5 / o-series have much larger
/// caps (128K+), so we leave their requests untouched.
pub(crate) fn max_output_tokens_for(model: &str) -> u32 {
    if model_requires_max_completion_tokens(model) {
        // gpt-5 / o-series: documented at 128K output. Leave the
        // caller's value alone — they know what they're asking for.
        u32::MAX
    } else {
        // gpt-4o family published cap. gpt-4-turbo / gpt-3.5 have a
        // lower cap (4096) and will still 400 — this is intentional;
        // they're outside the strict-mode target audience.
        16_384
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OpenAiProvider, RequestDialect, enforce_strict_object_schemas,
        model_requires_max_completion_tokens, normalize_openai_base,
    };
    use crate::types::{ChatMessage, ChatRequest, Role};
    use schemars::JsonSchema;
    use secrecy::SecretString;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    fn provider_for(model: &str) -> OpenAiProvider {
        OpenAiProvider::new(SecretString::new("test-key".into()), model).unwrap()
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "hi".to_string(),
            }],
            max_tokens: 256,
            temperature: None,
        }
    }

    #[test]
    fn enforce_strict_injects_additional_properties_false_on_root() {
        let mut schema = json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"]
        });
        enforce_strict_object_schemas(&mut schema);
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn enforce_strict_recurses_into_nested_objects() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "page": {
                    "type": "object",
                    "properties": { "title": { "type": "string" } }
                },
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "name": { "type": "string" } }
                    }
                }
            }
        });
        enforce_strict_object_schemas(&mut schema);
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["page"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            schema["properties"]["tags"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn enforce_strict_fills_required_with_all_property_keys() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        });
        enforce_strict_object_schemas(&mut schema);
        let required = schema["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"title"));
        assert!(names.contains(&"body"));
        assert!(names.contains(&"tags"));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn enforce_strict_overwrites_incomplete_required() {
        // OpenAI strict mode rejects partial `required` arrays — even an
        // explicit subset from the caller. Optionality at the value site
        // (nullable union types) is the only supported escape hatch.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" }
            },
            "required": ["a"]
        });
        enforce_strict_object_schemas(&mut schema);
        let required = schema["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn enforce_strict_strips_sibling_keywords_next_to_ref() {
        // OpenAI's structured-output validator rejects any sibling
        // keyword next to `$ref` with a 400. schemars 1.x emits a
        // field-level `description` next to `$ref` whenever a
        // doc-commented field is typed as an external enum (e.g.
        // `tier: Tier` in `ConsolidatedPageUpdate`). Without this
        // strip, `memory_consolidate multi_page=true` 400s on every
        // call against gpt-4o-mini. In our generated schemas those
        // siblings are annotations, not validation constraints.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "tier": {
                    "$ref": "#/$defs/Tier",
                    "description": "Tier classification."
                }
            }
        });
        enforce_strict_object_schemas(&mut schema);
        let tier = &schema["properties"]["tier"];
        assert_eq!(tier["$ref"], json!("#/$defs/Tier"));
        assert!(
            tier.get("description").is_none(),
            "description must be stripped from a $ref node"
        );
        assert_eq!(
            tier.as_object().unwrap().len(),
            1,
            "only $ref should remain on the node"
        );
    }

    #[test]
    fn enforce_strict_renames_oneof_to_anyof() {
        // OpenAI structured-output strict mode rejects `oneOf` outright
        // ("In context=(), 'oneOf' is not permitted") while accepting
        // `anyOf`. schemars 1.x emits `oneOf` for every Rust enum with
        // tagged variants — e.g. the `Tier` and `PageKind` enums under
        // `$defs` in `ConsolidatedBatch`. For closed enum sets where
        // exactly one branch matches per value, `anyOf` is semantically
        // equivalent (no two branches overlap), so the rewrite is
        // lossless.
        let mut schema = json!({
            "type": "object",
            "$defs": {
                "Tier": {
                    "oneOf": [
                        { "type": "string", "const": "working" },
                        { "type": "string", "const": "episodic" }
                    ]
                }
            },
            "properties": { "tier": { "$ref": "#/$defs/Tier" } }
        });
        enforce_strict_object_schemas(&mut schema);
        let tier_def = &schema["$defs"]["Tier"];
        assert!(
            tier_def.get("oneOf").is_none(),
            "oneOf must be rewritten away"
        );
        let any_of = tier_def.get("anyOf").expect("oneOf must become anyOf");
        assert_eq!(any_of.as_array().unwrap().len(), 2);
    }

    #[test]
    fn enforce_strict_normalizes_schemars_enum_refs() {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        struct Fixture {
            /// Doc-commented enum field, matching the schemars shape that
            /// triggered OpenAI's `$ref` sibling rejection.
            tier: FixtureTier,
            /// Array of the same enum, covering `$ref` under `items`.
            tiers: Vec<FixtureTier>,
        }

        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        enum FixtureTier {
            Working,
            Episodic,
        }

        let mut schema = serde_json::to_value(schemars::schema_for!(Fixture)).unwrap();
        enforce_strict_object_schemas(&mut schema);

        assert_no_one_of(&schema);
        assert_no_ref_siblings(&schema);
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn enforce_strict_strips_ref_siblings_inside_array_items() {
        // OpenAI applies the same `$ref` sibling restriction anywhere a
        // ref appears, not just on direct object properties. schemars
        // emits this shape inside `items` for `Vec<EnumType>` too.
        let mut schema = json!({
            "type": "array",
            "items": {
                "$ref": "#/$defs/Tier",
                "description": "Each element classified."
            }
        });
        enforce_strict_object_schemas(&mut schema);
        let items = &schema["items"];
        assert_eq!(items["$ref"], json!("#/$defs/Tier"));
        assert!(items.get("description").is_none());
    }

    fn assert_no_one_of(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                assert!(map.get("oneOf").is_none(), "oneOf remains in {value}");
                for child in map.values() {
                    assert_no_one_of(child);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    assert_no_one_of(child);
                }
            }
            _ => {}
        }
    }

    fn assert_no_ref_siblings(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if map.contains_key("$ref") {
                    assert_eq!(map.len(), 1, "$ref has siblings in {value}");
                }
                for child in map.values() {
                    assert_no_ref_siblings(child);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    assert_no_ref_siblings(child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn enforce_strict_overwrites_caller_additional_properties_true() {
        // OpenAI strict mode requires `additionalProperties: false` on
        // every object node — preserving an explicit `true` would
        // re-introduce the 400 this function exists to prevent. The
        // PR-review version of this test had the opposite assertion
        // and was incompatible with the function's own contract.
        let mut schema = json!({
            "type": "object",
            "properties": { "anything": { "type": "string" } },
            "additionalProperties": true
        });
        enforce_strict_object_schemas(&mut schema);
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "strict mode requires false; caller's true must be overwritten"
        );
    }

    #[test]
    fn enforce_strict_ignores_non_object_nodes() {
        let mut schema = json!({ "type": "string" });
        enforce_strict_object_schemas(&mut schema);
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn model_requires_max_completion_tokens_matches_gpt5_and_o_series() {
        assert!(model_requires_max_completion_tokens("gpt-5"));
        assert!(model_requires_max_completion_tokens("gpt-5-mini"));
        assert!(model_requires_max_completion_tokens("gpt-5.4-nano"));
        assert!(model_requires_max_completion_tokens("GPT-5"));
        assert!(model_requires_max_completion_tokens("o1-mini"));
        assert!(model_requires_max_completion_tokens("o3"));
        assert!(model_requires_max_completion_tokens("o4-mini"));
    }

    #[test]
    fn model_requires_max_completion_tokens_passes_gpt4_through() {
        assert!(!model_requires_max_completion_tokens("gpt-4o-mini"));
        assert!(!model_requires_max_completion_tokens("gpt-4-turbo"));
        assert!(!model_requires_max_completion_tokens("gpt-3.5-turbo"));
        assert!(!model_requires_max_completion_tokens("claude-haiku-4-5"));
    }

    #[test]
    fn build_request_uses_max_tokens_for_gpt4() {
        let p = provider_for("gpt-4o-mini");
        let req_input = chat_request();
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_tokens"], json!(256));
        assert!(json.get("max_completion_tokens").is_none());
    }

    #[test]
    fn build_request_uses_max_completion_tokens_for_gpt5() {
        let p = provider_for("gpt-5.4-nano");
        let req_input = chat_request();
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_completion_tokens"], json!(256));
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn build_request_caps_huge_max_tokens_on_gpt4o() {
        // Bootstrap requests 64K output to avoid mid-JSON truncation on
        // Anthropic Haiku-class models. OpenAI gpt-4o family caps at
        // 16384 and rejects above; cap silently so the caller doesn't
        // need to know per-model limits.
        let p = provider_for("gpt-4o-mini");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 64_000,
            temperature: None,
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_tokens"], json!(16_384));
    }

    #[test]
    fn build_request_omits_temperature_for_gpt5() {
        // gpt-5 / o-series reject any non-default temperature. The
        // `Official` dialect must omit the field so the API uses its
        // model-specific default.
        let p = provider_for("gpt-5.4-nano");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 256,
            temperature: Some(0.2),
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert!(
            json.get("temperature").is_none(),
            "temperature must be omitted for gpt-5/o-series under the Official dialect"
        );
    }

    #[test]
    fn build_request_keeps_temperature_for_gpt4() {
        // gpt-4 family accepts any temperature; forwarding the
        // caller's value is the legacy behaviour and stays.
        let p = provider_for("gpt-4o-mini");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 256,
            temperature: Some(0.2),
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        let temp = json["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.2).abs() < 1e-6,
            "temperature must be ~0.2, got {temp}"
        );
    }

    #[test]
    fn build_request_compat_dialect_keeps_max_tokens_and_temperature() {
        // `Compat` (Ollama / vLLM / LM Studio) speaks the legacy
        // wire format only — even when the model id starts with
        // `gpt-5*`, because the local engine doesn't implement the
        // new dialect. Akita flagged this regression in PR review.
        let p = OpenAiProvider::new(SecretString::new("dummy".into()), "gpt-5-mini")
            .unwrap()
            .with_dialect(RequestDialect::Compat);
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 64_000,
            temperature: Some(0.2),
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json["max_tokens"],
            json!(64_000),
            "compat dialect must use legacy max_tokens, uncapped"
        );
        assert!(
            json.get("max_completion_tokens").is_none(),
            "compat dialect must not emit max_completion_tokens"
        );
        let temp = json["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.2).abs() < 1e-6,
            "compat dialect must forward temperature unchanged, got {temp}"
        );
    }

    #[test]
    fn build_request_does_not_cap_gpt5() {
        // Reasoning models have a much larger output cap (128K+); leave
        // the caller's value alone.
        let p = provider_for("gpt-5.4-nano");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 64_000,
            temperature: None,
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_completion_tokens"], json!(64_000));
    }

    #[test]
    fn normalize_openai_base_chat_completions() {
        let ep = "chat/completions";

        assert_eq!(
            normalize_openai_base("https://api.openai.com", ep),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("https://api.openai.com/", ep),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("https://openrouter.ai/api/v1", ep),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("http://localhost:11434/v1", ep),
            "http://localhost:11434/v1/chat/completions"
        );
        // /v123 must not be treated as a version segment.
        assert_eq!(
            normalize_openai_base("https://example.com/v123", ep),
            "https://example.com/v123/v1/chat/completions"
        );
        // Z.AI-style: non-v1 version segment in the path.
        assert_eq!(
            normalize_openai_base("https://api.z.ai/api/coding/paas/v4", ep),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
        // Full endpoint URL already provided (Z.AI or GitHub Copilot style).
        assert_eq!(
            normalize_openai_base("https://api.z.ai/api/coding/paas/v4/chat/completions", ep),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("https://api.githubcopilot.com/chat/completions", ep),
            "https://api.githubcopilot.com/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_embeddings() {
        let ep = "embeddings";

        assert_eq!(
            normalize_openai_base("https://api.openai.com", ep),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("https://openrouter.ai/api/v1", ep),
            "https://openrouter.ai/api/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("http://localhost:11434/v1", ep),
            "http://localhost:11434/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("https://example.com/v123", ep),
            "https://example.com/v123/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("https://api.z.ai/api/coding/paas/v4", ep),
            "https://api.z.ai/api/coding/paas/v4/embeddings"
        );
    }
}
