//! Versioned, bounded JSONL provider transport for UpAgent.
//!
//! The process owns no credentials. It accepts a provider-neutral turn or an
//! image-generation request on stdin, runs one read-only/text-only Vergerail
//! operation against the explicitly configured managed Codex home, and emits
//! one strict JSON response on stdout. A caller that cannot observe the
//! result must resolve the outcome; this binary never retries a request.

#![forbid(unsafe_code)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::io::{self, Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;
use vergerail::{Codex, CodexConfig, ReasoningEffort, RuntimePackage, SessionOptions, TurnStatus};

const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 512 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const MAX_MESSAGES: usize = 128;
const MAX_TOOLS: usize = 64;
const MAX_TOOL_CALLS: usize = 64;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_MODEL_SESSION_BYTES: usize = MAX_TEXT_BYTES + 256 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROVIDER_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

type ProviderResult<T> = Result<T, ProviderFailure>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderRequest {
    schema_version: u32,
    request_id: String,
    operation: ProviderOperation,
    #[serde(default)]
    messages: Vec<ProviderMessage>,
    #[serde(default)]
    tools: Vec<ProviderTool>,
    #[serde(default)]
    reasoning: ProviderReasoning,
    timeout_ms: u64,
    maximum_response_bytes: usize,
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProviderOperation {
    ModelTurn,
    ImageGenerate,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProviderReasoning {
    Off,
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderMessage {
    role: ProviderMessageRole,
    content: String,
    #[serde(default)]
    tool_calls: Vec<ProviderToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProviderMessageRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(default = "default_true")]
    strict: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ModelResponseBody {
    text: String,
    tool_calls: Vec<ProviderToolCall>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelResponse {
    schema_version: u32,
    request_id: String,
    operation: ProviderOperation,
    text: String,
    tool_calls: Vec<ProviderToolCall>,
    usage: Option<ProviderUsage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageResponse {
    schema_version: u32,
    request_id: String,
    operation: ProviderOperation,
    image: ProviderImage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderImage {
    media_type: &'static str,
    base64: String,
    byte_length: usize,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FailureResponse {
    schema_version: u32,
    request_id: String,
    ok: bool,
    error: ProviderFailureBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderFailureBody {
    code: &'static str,
    message: String,
    retryable: bool,
}

#[derive(Debug, Clone)]
struct ProviderFailure {
    code: &'static str,
    message: String,
    retryable: bool,
}

#[derive(Debug, Clone)]
struct ProviderConfig {
    codex_home: PathBuf,
    runtime_package: PathBuf,
    home_owner: String,
    model: String,
    workspace: PathBuf,
}

impl ProviderConfig {
    fn from_environment() -> ProviderResult<Self> {
        let codex_home = required_path("VERGERAIL_CODEX_HOME")?;
        let runtime_package = required_path("VERGERAIL_CODEX_PACKAGE")?;
        let home_owner = required_string("VERGERAIL_HOME_OWNER")?;
        let model = required_string("VERGERAIL_MODEL")?;
        let workspace = required_path("VERGERAIL_WORKSPACE")?;
        if !codex_home.is_absolute() || !runtime_package.is_absolute() || !workspace.is_absolute() {
            return Err(failure(
                "invalid_configuration",
                "managed home, runtime package, and workspace must be absolute paths",
                false,
            ));
        }
        if !valid_home_owner(&home_owner) {
            return Err(failure(
                "invalid_configuration",
                "home owner must match [a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?",
                false,
            ));
        }
        if model.len() > 160 {
            return Err(failure(
                "invalid_configuration",
                "model identifier exceeds the configured bound",
                false,
            ));
        }
        Ok(Self {
            codex_home,
            runtime_package,
            home_owner,
            model,
            workspace,
        })
    }
}

fn default_true() -> bool {
    true
}

fn required_string(name: &str) -> ProviderResult<String> {
    let value = env::var(name).map_err(|_| {
        failure(
            "invalid_configuration",
            &format!("{name} must be explicitly configured"),
            false,
        )
    })?;
    if value.trim().is_empty() || value.len() > 4_096 || value.contains('\0') {
        return Err(failure(
            "invalid_configuration",
            &format!("{name} must be a bounded non-empty value"),
            false,
        ));
    }
    Ok(value)
}

fn required_path(name: &str) -> ProviderResult<PathBuf> {
    Ok(PathBuf::from(required_string(name)?))
}

fn valid_home_owner(value: &str) -> bool {
    let bytes = value.as_bytes();
    let valid_alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    match bytes {
        [only] => valid_alnum(*only),
        [first, middle @ .., last] if (2..=64).contains(&bytes.len()) => {
            valid_alnum(*first)
                && valid_alnum(*last)
                && middle
                    .iter()
                    .all(|byte| valid_alnum(*byte) || *byte == b'-')
        }
        _ => false,
    }
}

fn failure(code: &'static str, message: &str, retryable: bool) -> ProviderFailure {
    ProviderFailure {
        code,
        message: message.to_owned(),
        retryable,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let raw = read_request();
    let request_id = raw
        .as_ref()
        .map(|request| request.request_id.clone())
        .unwrap_or_default();
    let parsed = raw.and_then(validate_request);
    let result = match parsed {
        Ok(request) => match ProviderConfig::from_environment() {
            Ok(config) => async_run_request(request, config).await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    let response = match result {
        Ok(Response::Model(response)) => serde_json::to_vec(&response),
        Ok(Response::Image(response)) => serde_json::to_vec(&response),
        Err(error) => serde_json::to_vec(&FailureResponse {
            schema_version: PROTOCOL_VERSION,
            request_id,
            ok: false,
            error: ProviderFailureBody {
                code: error.code,
                message: error.message,
                retryable: error.retryable,
            },
        }),
    };
    match response {
        Ok(bytes) => {
            let mut stdout = io::BufWriter::new(io::stdout().lock());
            let _ = stdout.write_all(&bytes);
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        }
        Err(_) => std::process::exit(2),
    }
}

fn read_request() -> ProviderResult<ProviderRequest> {
    let mut bytes = Vec::new();
    io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            failure(
                "invalid_request",
                &format!("cannot read request: {error}"),
                false,
            )
        })?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(failure(
            "resource_limit",
            "request exceeds the bounded JSONL frame limit",
            false,
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        failure(
            "invalid_request",
            &format!("request is not strict JSON: {error}"),
            false,
        )
    })
}

fn validate_request(request: ProviderRequest) -> ProviderResult<ProviderRequest> {
    if request.schema_version != 1 || request.request_id.trim().is_empty() {
        return Err(failure(
            "invalid_request",
            "schemaVersion must be 1 and requestId must be non-empty",
            false,
        ));
    }
    if request.request_id.len() > 128 {
        return Err(failure("invalid_request", "requestId is too long", false));
    }
    if request.timeout_ms == 0 || request.timeout_ms > MAX_PROVIDER_TIMEOUT_MS {
        return Err(failure(
            "invalid_request",
            "timeoutMs is outside the bounded provider range",
            false,
        ));
    }
    let maximum_response_cap = match request.operation {
        ProviderOperation::ModelTurn => MAX_TEXT_BYTES,
        ProviderOperation::ImageGenerate => MAX_IMAGE_BYTES,
    };
    if request.maximum_response_bytes == 0 || request.maximum_response_bytes > maximum_response_cap
    {
        return Err(failure(
            "invalid_request",
            "maximumResponseBytes is outside the bounded provider range",
            false,
        ));
    }
    if request.messages.len() > MAX_MESSAGES || request.tools.len() > MAX_TOOLS {
        return Err(failure(
            "resource_limit",
            "messages or tools exceed the bounded provider limit",
            false,
        ));
    }
    if request.operation == ProviderOperation::ImageGenerate {
        let prompt = request
            .prompt
            .as_deref()
            .ok_or_else(|| failure("invalid_request", "image_generate requires prompt", false))?;
        validate_text(prompt, "prompt")?;
        if !request.messages.is_empty() || !request.tools.is_empty() {
            return Err(failure(
                "invalid_request",
                "image_generate does not accept model messages or tools",
                false,
            ));
        }
    } else if request.prompt.is_some() {
        return Err(failure(
            "invalid_request",
            "model_turn does not accept prompt; provide messages",
            false,
        ));
    } else if request.messages.is_empty() {
        return Err(failure(
            "invalid_request",
            "model_turn requires at least one message",
            false,
        ));
    }
    for message in &request.messages {
        validate_message(message)?;
    }
    let mut names = std::collections::BTreeSet::new();
    for tool in &request.tools {
        validate_text(&tool.name, "tool name")?;
        validate_text(&tool.description, "tool description")?;
        if !tool.input_schema.is_object() {
            return Err(failure(
                "invalid_request",
                "tool inputSchema must be a JSON object",
                false,
            ));
        }
        if !names.insert(tool.name.clone()) {
            return Err(failure(
                "invalid_request",
                "tool names must be unique",
                false,
            ));
        }
    }
    Ok(request)
}

fn validate_text(value: &str, label: &str) -> ProviderResult<()> {
    if value.is_empty() || value.len() > MAX_MESSAGE_BYTES || value.contains('\0') {
        return Err(failure(
            "invalid_request",
            &format!("{label} must be a bounded non-empty UTF-8 value"),
            false,
        ));
    }
    Ok(())
}

fn validate_message(message: &ProviderMessage) -> ProviderResult<()> {
    if message.content.len() > MAX_MESSAGE_BYTES {
        return Err(failure(
            "resource_limit",
            "message content exceeds the bounded byte limit",
            false,
        ));
    }
    if message.role == ProviderMessageRole::Assistant {
        if message.content.is_empty() && message.tool_calls.is_empty() {
            return Err(failure(
                "invalid_request",
                "assistant messages require content or at least one tool call",
                false,
            ));
        }
    } else if message.content.is_empty() {
        return Err(failure(
            "invalid_request",
            "non-assistant messages require non-empty content",
            false,
        ));
    }
    if message.role == ProviderMessageRole::Tool {
        if message.tool_call_id.as_deref().is_none_or(str::is_empty)
            || message.tool_name.as_deref().is_none_or(str::is_empty)
        {
            return Err(failure(
                "invalid_request",
                "tool messages require non-empty toolCallId and toolName",
                false,
            ));
        }
        if !message.tool_calls.is_empty() {
            return Err(failure(
                "invalid_request",
                "tool messages cannot contain tool calls",
                false,
            ));
        }
    } else {
        if message.tool_call_id.is_some() || message.tool_name.is_some() {
            return Err(failure(
                "invalid_request",
                "only tool messages may contain toolCallId and toolName",
                false,
            ));
        }
        if message.role != ProviderMessageRole::Assistant && !message.tool_calls.is_empty() {
            return Err(failure(
                "invalid_request",
                "only assistant messages may contain tool calls",
                false,
            ));
        }
    }
    if message.tool_calls.len() > MAX_TOOL_CALLS {
        return Err(failure(
            "resource_limit",
            "message tool calls exceed the bounded provider limit",
            false,
        ));
    }
    for call in &message.tool_calls {
        validate_tool_call(call)?;
    }
    Ok(())
}

fn validate_tool_call(call: &ProviderToolCall) -> ProviderResult<()> {
    validate_text(&call.id, "tool call id")?;
    validate_text(&call.name, "tool call name")?;
    if !call.arguments.is_object() {
        return Err(failure(
            "invalid_request",
            "tool call arguments must be a JSON object",
            false,
        ));
    }
    Ok(())
}

enum Response {
    Model(ModelResponse),
    Image(ImageResponse),
}

async fn async_run_request(
    request: ProviderRequest,
    config: ProviderConfig,
) -> ProviderResult<Response> {
    let runtime = RuntimePackage::pinned(&config.runtime_package).map_err(|error| {
        failure(
            "runtime_verification",
            &format!("pinned runtime verification failed: {error}"),
            false,
        )
    })?;
    let enable_images = request.operation == ProviderOperation::ImageGenerate;
    let codex = connect(runtime, &config, enable_images).await?;
    let response = match request.operation {
        ProviderOperation::ModelTurn => run_model_turn(&codex, &config, &request).await?,
        ProviderOperation::ImageGenerate => run_image_generation(&codex, &config, &request).await?,
    };
    codex
        .shutdown()
        .await
        .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
    Ok(response)
}

async fn connect(
    runtime: RuntimePackage,
    config: &ProviderConfig,
    enable_images: bool,
) -> ProviderResult<Codex> {
    Codex::connect(
        CodexConfig::new(runtime, &config.codex_home)
            .with_home_owner(&config.home_owner)
            .with_image_generation(enable_images)
            .with_max_frame_bytes(MAX_FRAME_BYTES)
            .map_err(|error| failure("invalid_configuration", &error.to_string(), false))?,
    )
    .await
    .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))
}

async fn run_model_turn(
    codex: &Codex,
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> ProviderResult<Response> {
    let rendered = render_model_prompt(request)?;
    let output_schema = model_output_schema(request);
    let mut options = SessionOptions::read_only(&config.workspace)
        .with_model(&config.model)
        .with_reasoning(map_reasoning(request.reasoning))
        .with_turn_timeout(Duration::from_millis(request.timeout_ms))
        .with_maximum_output_bytes(
            request
                .maximum_response_bytes
                .saturating_add(256 * 1024)
                .min(MAX_MODEL_SESSION_BYTES),
        )
        .with_output_schema(output_schema)
        .text_only();
    if let Some(base) = rendered.base_instructions {
        options = options.with_base_instructions(base);
    }
    if let Some(developer) = rendered.developer_instructions {
        options = options.with_developer_instructions(developer);
    }
    let result = codex
        .run(rendered.prompt, options)
        .await
        .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
    if result.status != TurnStatus::Completed || !result.image_generations.is_empty() {
        return Err(failure(
            "provider_failed",
            "model turn did not complete as text-only output",
            false,
        ));
    }
    let body: ModelResponseBody = serde_json::from_str(result.text.trim()).map_err(|error| {
        failure(
            "invalid_provider_output",
            &format!("model output did not match the strict response schema: {error}"),
            false,
        )
    })?;
    if body.text.len() > request.maximum_response_bytes || body.tool_calls.len() > MAX_TOOL_CALLS {
        return Err(failure(
            "resource_limit",
            "structured model response exceeded the configured bound",
            false,
        ));
    }
    validate_returned_tools(&request.tools, &body.tool_calls)?;
    Ok(Response::Model(ModelResponse {
        schema_version: PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        operation: ProviderOperation::ModelTurn,
        text: body.text,
        tool_calls: body.tool_calls,
        usage: result.usage.map(|usage| ProviderUsage {
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            total_tokens: Some(usage.total_tokens),
        }),
    }))
}

async fn run_image_generation(
    codex: &Codex,
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> ProviderResult<Response> {
    let prompt = request
        .prompt
        .as_deref()
        .ok_or_else(|| failure("invalid_request", "image_generate requires prompt", false))?;
    let session = codex
        .session(
            SessionOptions::read_only(&config.workspace)
                .with_model(&config.model)
                .with_reasoning(map_reasoning(request.reasoning))
                .with_turn_timeout(Duration::from_millis(request.timeout_ms))
                .with_maximum_output_bytes(MAX_IMAGE_BYTES * 2)
                .image_only()
                .with_developer_instructions(
                    "Use image generation exactly once. Do not use shell, file, web, app, plugin, browser, computer-use, or subagent tools. Return no textual answer.",
                ),
        )
        .await
        .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
    let verification = async {
        let result = session
            .start(prompt)
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?
            .wait()
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
        if result.status != TurnStatus::Completed
            || !result.text.trim().is_empty()
            || result.image_generations.len() != 1
        {
            return Err(failure(
                "invalid_provider_output",
                "image turn did not complete with exactly one image and no textual output",
                false,
            ));
        }
        let image = &result.image_generations[0];
        if image.status() != "completed"
            || image.failure().is_some()
            || image.result_base64().is_empty()
        {
            return Err(failure(
                "image_generation_failed",
                "image generation returned a non-completed lifecycle state",
                false,
            ));
        }
        let bytes = BASE64_STANDARD
            .decode(image.result_base64())
            .map_err(|error| {
                failure(
                    "invalid_provider_output",
                    &format!("image result was not valid base64: {error}"),
                    false,
                )
            })?;
        if bytes.len() > MAX_IMAGE_BYTES || bytes.len() > request.maximum_response_bytes {
            return Err(failure(
                "resource_limit",
                "decoded image exceeds the bounded output limit",
                false,
            ));
        }
        let (width, height) = validate_png(&bytes)?;
        let audit = session
            .audit_turn(&result.turn_id)
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
        if !audit.commands.is_empty() || !audit.file_changes.is_empty() {
            return Err(failure(
                "provider_failed",
                "image turn recorded a command or file change outside the image item",
                false,
            ));
        }
        if audit.other_item_types.iter().any(|item_type| {
            !matches!(
                item_type.as_str(),
                "userMessage" | "agentMessage" | "reasoning"
            )
        }) {
            return Err(failure(
                "provider_failed",
                "image turn recorded an unsupported non-image item",
                false,
            ));
        }
        if audit.image_generations.as_slice() != result.image_generations.as_slice() {
            return Err(failure(
                "provider_failed",
                "image turn audit did not match the retained image item",
                false,
            ));
        }
        Ok(ProviderImage {
            media_type: "image/png",
            base64: BASE64_STANDARD.encode(&bytes),
            byte_length: bytes.len(),
            width,
            height,
        })
    }
    .await;
    let close = session.close().await;
    match (verification, close) {
        (Ok(image), Ok(())) => Ok(Response::Image(ImageResponse {
            schema_version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            operation: ProviderOperation::ImageGenerate,
            image,
        })),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(classify_vergerail_error(&error.to_string(), error.kind())),
        (Err(error), Err(close_error)) => Err(failure(
            "provider_failed",
            &format!(
                "image operation failed and cleanup failed: {}; {}",
                error.message, close_error
            ),
            false,
        )),
    }
}

struct RenderedModelPrompt {
    prompt: String,
    base_instructions: Option<String>,
    developer_instructions: Option<String>,
}

fn render_model_prompt(request: &ProviderRequest) -> ProviderResult<RenderedModelPrompt> {
    let mut base = Vec::new();
    let mut developer = Vec::new();
    let mut transcript = Vec::new();
    for message in &request.messages {
        match message.role {
            ProviderMessageRole::System => base.push(message.content.clone()),
            ProviderMessageRole::Developer => developer.push(message.content.clone()),
            ProviderMessageRole::User
            | ProviderMessageRole::Assistant
            | ProviderMessageRole::Tool => transcript.push(message),
        }
    }
    let envelope = serde_json::json!({
        "messages": transcript,
        "tools": request.tools,
    });
    let encoded = serde_json::to_string(&envelope).map_err(|error| {
        failure(
            "invalid_request",
            &format!("could not encode provider-neutral turn: {error}"),
            false,
        )
    })?;
    Ok(RenderedModelPrompt {
        prompt: format!(
            "Process the provider-neutral conversation below. Reply with exactly one compact JSON object matching the native output schema, with no markdown or surrounding text. Tool calls must use only advertised tools and object arguments.\n{encoded}"
        ),
        base_instructions: (!base.is_empty()).then(|| base.join("\n\n")),
        developer_instructions: (!developer.is_empty()).then(|| developer.join("\n\n")),
    })
}

fn model_output_schema(request: &ProviderRequest) -> Value {
    let names = request
        .tools
        .iter()
        .map(|tool| Value::String(tool.name.clone()))
        .collect::<Vec<_>>();
    let mut name_schema = serde_json::json!({"type": "string"});
    let maximum_tool_calls = if names.is_empty() { 0 } else { MAX_TOOL_CALLS };
    if !names.is_empty() {
        name_schema["enum"] = Value::Array(names);
    }
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["text", "toolCalls"],
        "properties": {
            "text": {"type": "string", "maxLength": MAX_TEXT_BYTES},
            "toolCalls": {
                "type": "array",
                "maxItems": maximum_tool_calls,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "name", "arguments"],
                    "properties": {
                        "id": {"type": "string", "maxLength": MAX_MESSAGE_BYTES},
                        "name": {
                            "allOf": [name_schema],
                            "maxLength": MAX_MESSAGE_BYTES
                        },
                        "arguments": {"type": "object"}
                    }
                }
            }
        }
    })
}

fn validate_returned_tools(
    tools: &[ProviderTool],
    calls: &[ProviderToolCall],
) -> ProviderResult<()> {
    let advertised = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut ids = std::collections::BTreeSet::new();
    for call in calls {
        validate_tool_call(call)?;
        if !advertised.contains(call.name.as_str()) {
            return Err(failure(
                "invalid_provider_output",
                "provider returned a tool outside the advertised set",
                false,
            ));
        }
        if !ids.insert(&call.id) {
            return Err(failure(
                "invalid_provider_output",
                "provider returned duplicate tool call ids",
                false,
            ));
        }
    }
    Ok(())
}

fn map_reasoning(reasoning: ProviderReasoning) -> ReasoningEffort {
    match reasoning {
        ProviderReasoning::Off => ReasoningEffort::None,
        ProviderReasoning::Low => ReasoningEffort::Low,
        ProviderReasoning::Medium => ReasoningEffort::Medium,
        ProviderReasoning::High => ReasoningEffort::High,
    }
}

fn validate_png(bytes: &[u8]) -> ProviderResult<(u32, u32)> {
    if bytes.len() < 33 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || &bytes[12..16] != b"IHDR" {
        return Err(failure(
            "invalid_provider_output",
            "image generation must return a PNG raster",
            false,
        ));
    }
    let width_bytes: [u8; 4] = bytes[16..20].try_into().map_err(|_| {
        failure(
            "invalid_provider_output",
            "PNG width header is invalid",
            false,
        )
    })?;
    let height_bytes: [u8; 4] = bytes[20..24].try_into().map_err(|_| {
        failure(
            "invalid_provider_output",
            "PNG height header is invalid",
            false,
        )
    })?;
    let width = u32::from_be_bytes(width_bytes);
    let height = u32::from_be_bytes(height_bytes);
    if width == 0 || height == 0 || width > 8_192 || height > 8_192 {
        return Err(failure(
            "resource_limit",
            "PNG dimensions exceed the bounded raster limit",
            false,
        ));
    }
    if u64::from(width) * u64::from(height) > 8_192 * 8_192 {
        return Err(failure(
            "resource_limit",
            "PNG decoded pixel count exceeds the bounded raster limit",
            false,
        ));
    }
    Ok((width, height))
}

fn classify_vergerail_error(message: &str, kind: vergerail::ErrorKind) -> ProviderFailure {
    let code = match kind {
        vergerail::ErrorKind::Timeout => "timeout",
        vergerail::ErrorKind::OutcomeUnknown
        | vergerail::ErrorKind::Disconnected
        | vergerail::ErrorKind::ConsumerLagged => "resolution_required",
        vergerail::ErrorKind::Authentication => "authentication_required",
        vergerail::ErrorKind::RuntimeVerification => "runtime_verification",
        vergerail::ErrorKind::InvalidInput => "invalid_request",
        _ => "provider_failed",
    };
    ProviderFailure {
        code,
        message: message.to_owned(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(operation: ProviderOperation) -> ProviderRequest {
        ProviderRequest {
            schema_version: 1,
            request_id: "request-1".to_owned(),
            operation,
            messages: vec![ProviderMessage {
                role: ProviderMessageRole::User,
                content: "hello".to_owned(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                is_error: false,
            }],
            tools: Vec::new(),
            reasoning: ProviderReasoning::Medium,
            timeout_ms: 1_000,
            maximum_response_bytes: 1_024,
            prompt: None,
        }
    }

    #[test]
    fn request_validation_is_strict_and_bounded() {
        let valid = validate_request(request(ProviderOperation::ModelTurn))
            .expect("model turn with provider-neutral messages is valid");
        assert_eq!(valid.schema_version, 1);

        let mut image = request(ProviderOperation::ImageGenerate);
        image.messages.clear();
        image.prompt = Some("make a square png".to_owned());
        assert!(validate_request(image).is_ok());

        let mut large_image = request(ProviderOperation::ImageGenerate);
        large_image.messages.clear();
        large_image.prompt = Some("make a square png".to_owned());
        large_image.maximum_response_bytes = MAX_IMAGE_BYTES;
        assert!(validate_request(large_image).is_ok());

        let mut oversized = request(ProviderOperation::ModelTurn);
        oversized.maximum_response_bytes = MAX_TEXT_BYTES + 1;
        assert_eq!(
            validate_request(oversized).expect_err("bound").code,
            "invalid_request"
        );
    }

    #[test]
    fn strict_model_body_rejects_unknown_fields() {
        let result = serde_json::from_value::<ModelResponseBody>(json!({
            "text": "ok",
            "toolCalls": [],
            "usage": null,
            "markdown": "no"
        }));
        assert!(result.is_err());
    }

    #[test]
    fn png_header_and_decoded_pixel_bounds_are_enforced() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(validate_png(&png).unwrap(), (1, 1));
        png[16..20].copy_from_slice(&9_000u32.to_be_bytes());
        assert_eq!(
            validate_png(&png).expect_err("dimension bound").code,
            "resource_limit"
        );
    }

    #[test]
    fn protocol_schema_is_versioned() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
