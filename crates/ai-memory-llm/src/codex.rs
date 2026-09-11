//! Codex provider that reuses the Codex CLI-owned authentication file.

use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _,
};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::auth::CodexAuth;
use crate::codex_responses::{CODEX_RESPONSES_URL, CodexResponsesAuth, post_codex_responses};
use crate::error::{LlmError, LlmResult};
use crate::openai::{STRUCTURED_OUTPUT_SCHEMA_NAME, enforce_strict_object_schemas};
use crate::openai_oauth::{
    CodexResponsesRequest, CodexText, CodexTextFormat, build_request, extract_output_text,
    into_chat_response,
};
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, ExtraHeaders, ReasoningEffort};

const MAX_JSONL_LINE_BYTES: usize = 256 * 1024;
const MAX_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_RECOVERY_SECS: u64 = 30;

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: CodexTokenFields,
}

#[derive(Deserialize)]
struct CodexTokenFields {
    access_token: String,
    account_id: String,
}

#[derive(Clone)]
struct CodexCredentials {
    access_token: SecretString,
    account_id: String,
}

impl fmt::Debug for CodexCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexCredentials([REDACTED])")
    }
}

/// ChatGPT/Codex Responses provider backed by Codex CLI credentials.
pub struct CodexProvider {
    client: reqwest::Client,
    model: String,
    auth: CodexAuth,
    timeout: Duration,
    reasoning_effort: Option<ReasoningEffort>,
    extra_headers: ExtraHeaders,
    recovery: Mutex<()>,
    responses_url: String,
}

impl CodexProvider {
    /// Construct the provider and validate that Codex's auth file is usable.
    ///
    /// # Errors
    /// Returns a sanitized authentication error for missing or malformed auth.
    pub fn new(auth: CodexAuth, model: impl Into<String>) -> LlmResult<Self> {
        read_credentials(&auth.auth_file)?;
        Ok(Self {
            client: reqwest::Client::builder().build().map_err(LlmError::from)?,
            model: model.into(),
            auth,
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
            recovery: Mutex::new(()),
            responses_url: CODEX_RESPONSES_URL.into(),
        })
    }

    /// Override the Responses request and recovery ceiling.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// Set Responses API `reasoning.effort`.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Attach operator-configured headers to every Responses request.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        self.extra_headers = headers;
        self
    }

    #[cfg(test)]
    fn with_responses_url(mut self, url: impl Into<String>) -> Self {
        self.responses_url = url.into();
        self
    }

    async fn post_once(
        &self,
        credentials: &CodexCredentials,
        body: &CodexResponsesRequest<'_>,
    ) -> LlmResult<crate::openai_oauth::CodexResponsesResponse> {
        post_codex_responses(
            &self.client,
            &self.responses_url,
            self.timeout,
            &CodexResponsesAuth {
                access_token: &credentials.access_token,
                account_id: Some(&credentials.account_id),
            },
            &self.extra_headers,
            body,
        )
        .await
    }

    async fn post(
        &self,
        body: &CodexResponsesRequest<'_>,
    ) -> LlmResult<crate::openai_oauth::CodexResponsesResponse> {
        let initial = read_credentials(&self.auth.auth_file)?;
        match self.post_once(&initial, body).await {
            Err(LlmError::Provider { status: 401, .. }) => {}
            result => return result,
        }

        let mut current = read_credentials(&self.auth.auth_file)?;
        if same_access_token(&initial, &current) {
            let _guard = self.recovery.lock().await;
            current = read_credentials(&self.auth.auth_file)?;
            if same_access_token(&initial, &current) {
                recover_with_codex(&self.auth, self.timeout).await?;
                current = read_credentials(&self.auth.auth_file)?;
                if same_access_token(&initial, &current) {
                    return Err(codex_reauth_error(
                        "Codex completed recovery without replacing the access token",
                    ));
                }
            }
        }

        match self.post_once(&current, body).await {
            Err(LlmError::Provider { status: 401, .. }) => Err(codex_reauth_error(
                "Codex authentication was rejected after one recovery attempt",
            )),
            result => result,
        }
    }
}

#[async_trait]
impl LlmProvider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let response = self
            .post(&build_request(
                &self.model,
                &request,
                None,
                self.reasoning_effort,
            ))
            .await?;
        Ok(into_chat_response(response))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        enforce_strict_object_schemas(&mut schema);
        let response = self
            .post(&build_request(
                &self.model,
                &request,
                Some(CodexText {
                    format: CodexTextFormat::JsonSchema {
                        name: STRUCTURED_OUTPUT_SCHEMA_NAME.into(),
                        schema,
                        strict: true,
                    },
                }),
                self.reasoning_effort,
            ))
            .await?;
        serde_json::from_str(&extract_output_text(&response).unwrap_or_default())
            .map_err(LlmError::from)
    }
}

fn read_credentials(path: &Path) -> LlmResult<CodexCredentials> {
    let bytes = std::fs::read(path).map_err(|_| {
        codex_reauth_error(&format!(
            "Codex auth file is unavailable at {} (file storage is required; keyring-only and ephemeral storage are unsupported)",
            path.display()
        ))
    })?;
    let file: CodexAuthFile = serde_json::from_slice(&bytes).map_err(|_| {
        codex_reauth_error(&format!("Codex auth file is invalid at {}", path.display()))
    })?;
    let access_token = file.tokens.access_token.trim();
    let account_id = file.tokens.account_id.trim();
    if access_token.is_empty() || account_id.is_empty() {
        return Err(codex_reauth_error(
            "Codex auth file requires non-empty tokens.access_token and tokens.account_id",
        ));
    }
    Ok(CodexCredentials {
        access_token: SecretString::from(access_token.to_owned()),
        account_id: account_id.to_owned(),
    })
}

fn same_access_token(left: &CodexCredentials, right: &CodexCredentials) -> bool {
    left.access_token.expose_secret() == right.access_token.expose_secret()
}

fn codex_reauth_error(reason: &str) -> LlmError {
    LlmError::Auth(format!(
        "{reason}; run `codex login status` and authenticate Codex again if needed"
    ))
}

async fn recover_with_codex(auth: &CodexAuth, request_timeout: Duration) -> LlmResult<()> {
    let mut command = Command::new(&auth.executable);
    command
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(codex_home) = auth.auth_file.parent() {
        command.env("CODEX_HOME", codex_home);
    }
    let mut child = command.spawn().map_err(|_| {
        codex_reauth_error("Could not start `codex app-server --stdio` for recovery")
    })?;
    let timeout = request_timeout.min(Duration::from_secs(MAX_RECOVERY_SECS));
    let result = tokio::time::timeout(timeout, run_recovery_protocol(&mut child)).await;
    terminate_child(&mut child).await;
    match result {
        Ok(result) => result,
        Err(_) => Err(codex_reauth_error(
            "Codex authentication recovery timed out",
        )),
    }
}

async fn run_recovery_protocol(child: &mut Child) -> LlmResult<()> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| codex_reauth_error("Codex recovery process did not expose stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| codex_reauth_error("Codex recovery process did not expose stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| codex_reauth_error("Codex recovery process did not expose stderr"))?;
    let mut stdout = tokio::io::BufReader::new(stdout);
    let stderr_task = tokio::spawn(monitor_stderr(stderr));
    let protocol = async {
        let mut stdout_bytes = 0_usize;
        write_json_line(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {"name": "ai-memory", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {}
                }
            }),
        )
        .await?;
        read_expected_response(&mut stdout, 1, &mut stdout_bytes).await?;
        write_json_line(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        )
        .await?;
        write_json_line(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "account/read",
                "params": {"refreshToken": true}
            }),
        )
        .await?;
        read_expected_response(&mut stdout, 2, &mut stdout_bytes).await
    };
    tokio::pin!(protocol);
    tokio::pin!(stderr_task);
    tokio::select! {
        result = &mut protocol => {
            stderr_task.abort();
            result
        }
        stderr_result = &mut stderr_task => match stderr_result {
            Ok(result) => result,
            Err(_) => Err(codex_reauth_error("Codex recovery stderr monitor failed")),
        },
    }
}

async fn write_json_line(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &serde_json::Value,
) -> LlmResult<()> {
    let mut line = serde_json::to_vec(value).map_err(LlmError::from)?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(|_| codex_reauth_error("Could not write to Codex recovery process"))?;
    writer
        .flush()
        .await
        .map_err(|_| codex_reauth_error("Could not flush Codex recovery request"))
}

async fn read_expected_response(
    reader: &mut (impl AsyncBufRead + Unpin),
    expected_id: u64,
    total: &mut usize,
) -> LlmResult<()> {
    loop {
        let line = read_limited_line(reader, total).await?;
        let value: serde_json::Value = serde_json::from_slice(&line)
            .map_err(|_| codex_reauth_error("Codex recovery returned invalid JSON"))?;
        let Some(id) = value.get("id") else {
            continue;
        };
        if id.as_u64() != Some(expected_id) {
            return Err(codex_reauth_error(
                "Codex recovery returned an unexpected JSON-RPC response id",
            ));
        }
        if value.get("error").is_some() {
            return Err(codex_reauth_error(
                "Codex recovery returned a JSON-RPC error",
            ));
        }
        if value.get("result").is_none() {
            return Err(codex_reauth_error(
                "Codex recovery response did not contain a result",
            ));
        }
        return Ok(());
    }
}

async fn read_limited_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    total: &mut usize,
) -> LlmResult<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|_| codex_reauth_error("Could not read Codex recovery output"))?;
        if available.is_empty() {
            return Err(codex_reauth_error(
                "Codex recovery process ended before replying",
            ));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_JSONL_LINE_BYTES
            || total.saturating_add(take) > MAX_STDOUT_BYTES
        {
            return Err(codex_reauth_error(
                "Codex recovery output exceeded its limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        *total += take;
        if line.last() == Some(&b'\n') {
            return Ok(line);
        }
    }
}

async fn monitor_stderr(mut stderr: impl AsyncRead + Unpin) -> LlmResult<()> {
    let mut total = 0_usize;
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stderr
            .read(&mut buffer)
            .await
            .map_err(|_| codex_reauth_error("Could not read Codex recovery stderr"))?;
        if read == 0 {
            return Err(codex_reauth_error(
                "Codex recovery process ended before completing",
            ));
        }
        total = total.saturating_add(read);
        if total > MAX_STDERR_BYTES {
            return Err(codex_reauth_error(
                "Codex recovery stderr exceeded its limit",
            ));
        }
    }
}

async fn terminate_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use secrecy::ExposeSecret as _;
    use serde_json::json;
    use wiremock::matchers::{method, path as request_path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    fn write_auth(path: &Path, access: &str, account: &str) {
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "must-not-be-materialized",
                    "access_token": access,
                    "refresh_token": "must-not-be-materialized",
                    "account_id": account,
                    "future": {"accepted": true}
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn compile_fake_codex(dir: &Path) -> PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("support")
            .join("fake_codex.rs");
        let executable = dir.join(if cfg!(windows) {
            "fake-codex.exe"
        } else {
            "fake-codex"
        });
        let status = StdCommand::new("rustc")
            .arg(source)
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap();
        assert!(status.success());
        executable
    }

    fn completed_sse(text: &str) -> String {
        format!(
            "event: response.completed\ndata: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "output_text": text,
                    "model": "gpt-5.6-luna",
                    "usage": {"input_tokens": 3, "output_tokens": 2}
                }
            })
        )
    }

    #[derive(Clone)]
    struct RotateThenRespond {
        auth_path: PathBuf,
        calls: Arc<AtomicUsize>,
        second_status: u16,
    }

    #[derive(Clone)]
    struct UnauthorizedUntilRotated;

    impl Respond for UnauthorizedUntilRotated {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let authorization = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            if authorization == Some("Bearer new-token") {
                ResponseTemplate::new(200).set_body_string(completed_sse("ok"))
            } else {
                ResponseTemplate::new(401).set_body_string("expired")
            }
        }
    }

    impl Respond for RotateThenRespond {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let authorization = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            let account = request
                .headers
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok());
            if account != Some("account-secret") {
                return ResponseTemplate::new(500).set_body_string("missing account header");
            }
            let body: serde_json::Value = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(_) => return ResponseTemplate::new(500).set_body_string("invalid body"),
            };
            if body["model"] != "gpt-5.6-luna" || body["stream"] != true {
                return ResponseTemplate::new(500).set_body_string("wrong model or stream mode");
            }
            if body.get("reasoning").is_some() && body["reasoning"]["effort"] != json!("medium") {
                return ResponseTemplate::new(500).set_body_string("wrong reasoning effort");
            }
            for (name, expected) in [
                ("accept", "text/event-stream"),
                ("openai-beta", "responses=experimental"),
                ("originator", "codex_cli_rs"),
            ] {
                if request
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    != Some(expected)
                {
                    return ResponseTemplate::new(500)
                        .set_body_string(format!("wrong {name} header"));
                }
            }
            if call == 0 {
                if authorization != Some("Bearer old-token") {
                    return ResponseTemplate::new(500).set_body_string("wrong initial token");
                }
                write_auth(&self.auth_path, "new-token", "account-secret");
                return ResponseTemplate::new(401).set_body_string("expired");
            }
            if authorization != Some("Bearer new-token") {
                return ResponseTemplate::new(500).set_body_string("wrong reloaded token");
            }
            if self.second_status == 200 {
                ResponseTemplate::new(200).set_body_string(completed_sse("ok"))
            } else {
                ResponseTemplate::new(self.second_status).set_body_string("still rejected")
            }
        }
    }

    #[test]
    fn auth_parser_reads_only_required_non_empty_fields_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth(&path, "access-secret", "account-secret");
        let before = fs::read(&path).unwrap();

        let credentials = read_credentials(&path).unwrap();

        assert_eq!(credentials.access_token.expose_secret(), "access-secret");
        assert_eq!(credentials.account_id, "account-secret");
        assert_eq!(fs::read(&path).unwrap(), before);
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("account-secret"));
        assert!(!debug.contains("must-not-be-materialized"));
    }

    #[test]
    fn auth_parser_rejects_missing_invalid_and_empty_credentials_safely() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("missing.json", br#"{}"#.as_slice()),
            ("invalid.json", br#"{not-json"#.as_slice()),
            (
                "empty.json",
                br#"{"tokens":{"access_token":" ","account_id":"acct"}}"#.as_slice(),
            ),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            let error = read_credentials(&path).unwrap_err().to_string();
            assert!(error.contains("codex login status"));
            assert!(!error.contains("access_token\":\""));
        }
    }

    #[tokio::test]
    async fn protocol_ignores_notifications_and_rejects_unexpected_ids() {
        let (writer, reader) = tokio::io::duplex(4096);
        let mut reader = tokio::io::BufReader::new(reader);
        tokio::spawn(async move {
            let mut writer = writer;
            writer
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"method\":\"notice\"}\n{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n",
                )
                .await
                .unwrap();
        });
        let error = read_expected_response(&mut reader, 1, &mut 0)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unexpected JSON-RPC response id")
        );
    }

    #[tokio::test]
    async fn protocol_enforces_per_line_and_total_limits() {
        let oversized = vec![b'x'; MAX_JSONL_LINE_BYTES + 1];
        let mut reader = tokio::io::BufReader::new(oversized.as_slice());
        let error = read_limited_line(&mut reader, &mut 0).await.unwrap_err();
        assert!(error.to_string().contains("exceeded its limit"));

        let mut reader = tokio::io::BufReader::new(b"{}\n".as_slice());
        let mut total = MAX_STDOUT_BYTES;
        let error = read_limited_line(&mut reader, &mut total)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded its limit"));
    }

    #[tokio::test]
    async fn provider_reloads_rotated_token_and_retries_only_once() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_auth(&auth_path, "old-token", "account-secret");
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(request_path("/responses"))
            .respond_with(RotateThenRespond {
                auth_path: auth_path.clone(),
                calls: calls.clone(),
                second_status: 200,
            })
            .mount(&server)
            .await;
        let provider = CodexProvider::new(
            CodexAuth {
                auth_file: auth_path.clone(),
                executable: PathBuf::from("must-not-run"),
            },
            "gpt-5.6-luna",
        )
        .unwrap()
        .with_responses_url(format!("{}/responses", server.uri()))
        .with_reasoning_effort(Some(ReasoningEffort::Medium));

        let response = provider
            .complete(ChatRequest::user_prompt("test"))
            .await
            .unwrap();

        assert_eq!(response.text, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_does_not_loop_after_second_unauthorized_response() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_auth(&auth_path, "old-token", "account-secret");
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(request_path("/responses"))
            .respond_with(RotateThenRespond {
                auth_path: auth_path.clone(),
                calls: calls.clone(),
                second_status: 401,
            })
            .mount(&server)
            .await;
        let provider = CodexProvider::new(
            CodexAuth {
                auth_file: auth_path,
                executable: PathBuf::from("must-not-run"),
            },
            "gpt-5.6-luna",
        )
        .unwrap()
        .with_responses_url(format!("{}/responses", server.uri()));

        let error = provider
            .complete(ChatRequest::user_prompt("test"))
            .await
            .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(error.to_string().contains("codex login status"));
        assert!(!error.to_string().contains("old-token"));
        assert!(!error.to_string().contains("new-token"));
    }

    #[tokio::test]
    async fn provider_sends_and_parses_structured_responses() {
        #[derive(Clone)]
        struct StructuredResponder;
        impl Respond for StructuredResponder {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if body["text"]["format"]["type"] != "json_schema"
                    || body["text"]["format"]["strict"] != true
                {
                    return ResponseTemplate::new(500).set_body_string("missing JSON schema");
                }
                ResponseTemplate::new(200).set_body_string(completed_sse("{\"answer\":42}"))
            }
        }

        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_auth(&auth_path, "access-secret", "account-secret");
        Mock::given(method("POST"))
            .and(request_path("/responses"))
            .respond_with(StructuredResponder)
            .expect(1)
            .mount(&server)
            .await;
        let provider = CodexProvider::new(
            CodexAuth {
                auth_file: auth_path,
                executable: PathBuf::from("must-not-run"),
            },
            "gpt-5.6-luna",
        )
        .unwrap()
        .with_responses_url(format!("{}/responses", server.uri()));

        let value = provider
            .complete_structured_raw(
                ChatRequest::user_prompt("return JSON"),
                json!({
                    "type": "object",
                    "properties": {"answer": {"type": "integer"}}
                }),
            )
            .await
            .unwrap();

        assert_eq!(value, json!({"answer": 42}));
    }

    #[tokio::test]
    async fn rust_fake_codex_exercises_recovery_and_defensive_failures() {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let executable = compile_fake_codex(dir.path());
        let auth = CodexAuth {
            auth_file: auth_path.clone(),
            executable,
        };
        write_auth(&auth_path, "old-token", "account-secret");
        fs::write(dir.path().join("fake-mode"), "success").unwrap();

        recover_with_codex(&auth, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            read_credentials(&auth_path)
                .unwrap()
                .access_token
                .expose_secret(),
            "new-token"
        );

        for (mode, expected) in [
            ("wrong-id", "unexpected JSON-RPC response id"),
            ("invalid", "invalid JSON"),
            ("oversized", "exceeded its limit"),
            ("stderr", "stderr exceeded its limit"),
            ("exit", "ended before replying"),
        ] {
            fs::write(dir.path().join("fake-mode"), mode).unwrap();
            let error = recover_with_codex(&auth, Duration::from_secs(3))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "mode={mode}, error={error}");
        }

        fs::write(dir.path().join("fake-mode"), "sleep").unwrap();
        let error = recover_with_codex(&auth, Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn concurrent_unauthorized_calls_share_one_recovery() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_auth(&auth_path, "old-token", "account-secret");
        fs::write(dir.path().join("fake-mode"), "success").unwrap();
        Mock::given(method("POST"))
            .and(request_path("/responses"))
            .respond_with(UnauthorizedUntilRotated)
            .mount(&server)
            .await;
        let provider = Arc::new(
            CodexProvider::new(
                CodexAuth {
                    auth_file: auth_path,
                    executable: compile_fake_codex(dir.path()),
                },
                "gpt-5.6-luna",
            )
            .unwrap()
            .with_responses_url(format!("{}/responses", server.uri())),
        );

        let first = provider.complete(ChatRequest::user_prompt("first"));
        let second = provider.complete(ChatRequest::user_prompt("second"));
        let (first, second) = tokio::join!(first, second);

        assert_eq!(first.unwrap().text, "ok");
        assert_eq!(second.unwrap().text, "ok");
        assert_eq!(
            fs::read_to_string(dir.path().join("fake-invocations"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}
