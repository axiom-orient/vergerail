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
use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read as _, Write as _};
use std::path::Component;
use std::path::PathBuf;
use std::time::Duration;
use vergerail::{
    Codex, CodexConfig, DirectImageRequest, ImageBackground, ImageQuality, ImageSize,
    ReasoningEffort, RuntimePackage, SessionOptions, TurnStatus,
};

const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const MAX_MESSAGES: usize = 128;
const MAX_TOOLS: usize = 64;
const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 192;
const MAX_TOOL_NAME_BYTES: usize = 160;
const MAX_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_TOOL_ARGUMENTS_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
// The retained assistant body is JSON, so an 8 MiB decoded string can expand
// to six bytes per scalar. Keep the native output frame and retained-output
// bound aligned with UpAgent's 64 MiB model response frame.
const MAX_MODEL_SESSION_BYTES: usize = 64 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_FAILURE_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_FAILURE_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const JSON_STRING_MAX_EXPANSION: usize = 6;
const MODEL_RESPONSE_HEADROOM_BYTES: usize = 256 * 1024;
const MODEL_TOOL_CALL_WRAPPER_BYTES: usize = 128;
const MAX_IMAGE_BASE64_BYTES: usize = 4 * MAX_IMAGE_BYTES.div_ceil(3);
const _: () = assert!(MAX_IMAGE_BASE64_BYTES + MAX_FAILURE_RESPONSE_BYTES < MAX_IMAGE_FRAME_BYTES);

type ProviderResult<T> = Result<T, ProviderFailure>;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderRequest {
    schema_version: u32,
    request_id: String,
    operation: ProviderOperation,
    #[serde(default)]
    messages: Vec<ProviderMessage>,
    #[serde(default)]
    tools: Vec<ProviderTool>,
    reasoning: ProviderReasoning,
    timeout_ms: u64,
    maximum_response_bytes: usize,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    image_options: Option<ImageGenerationOptions>,
}

/// Explicit image controls sent by Vergerail's official image adapter.
///
/// The fields remain optional so omitted controls use the runtime defaults.
/// When a field is present, it is sent as an exact value to the official Images
/// endpoint; no prompt-based fallback is used.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ImageGenerationOptions {
    #[serde(default)]
    background: Option<ImageBackgroundOption>,
    #[serde(default)]
    size: Option<ImageSizeOption>,
    #[serde(default)]
    quality: Option<ImageQualityOption>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ImageBackgroundOption {
    Auto,
    Transparent,
    Opaque,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ImageQualityOption {
    Auto,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
enum ImageSizeOption {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "1024x1024")]
    Square,
    #[serde(rename = "1536x1024")]
    Landscape,
    #[serde(rename = "1024x1536")]
    Portrait,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProviderOperation {
    ModelTurn,
    ImageGenerate,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProviderReasoning {
    Off,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
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
struct ModelTextResponseBody {
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ModelToolResponseBody {
    text: String,
    tool_calls: BTreeMap<String, Vec<NativeToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NativeToolCall {
    id: String,
    arguments: Value,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    transparent_background: Option<bool>,
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
    runtime_package: PathBuf,
    codex_home: PathBuf,
    model: String,
    workspace: PathBuf,
}

impl ProviderConfig {
    fn from_environment() -> ProviderResult<Self> {
        let runtime_package = required_path("VERGERAIL_CODEX_PACKAGE")?;
        let codex_home = required_path("VERGERAIL_CODEX_HOME")?;
        let model = required_string("VERGERAIL_MODEL")?;
        let workspace = required_path("VERGERAIL_WORKSPACE")?;
        if !absolute_clean_path(&runtime_package)
            || !absolute_clean_path(&codex_home)
            || !absolute_clean_path(&workspace)
        {
            return Err(failure(
                "invalid_configuration",
                "runtime package, Codex home, and workspace must be absolute paths without parent traversal",
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
            runtime_package,
            codex_home,
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

fn absolute_clean_path(path: &std::path::Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().is_empty()
        && !path.to_string_lossy().contains('\0')
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

fn failure(code: &'static str, message: &str, retryable: bool) -> ProviderFailure {
    ProviderFailure {
        code,
        message: bounded_redacted_message(message),
        retryable,
    }
}

fn bounded_redacted_message(message: &str) -> String {
    let mut redacted = message.to_owned();
    for marker in [
        "bearer ",
        "access_token=",
        "authorization=",
        "client_secret=",
        "cookie=",
        "id_token=",
        "openai_api_key=",
        "password=",
        "refresh_token=",
        "secret=",
        "set-cookie=",
        "token=",
    ] {
        redacted = redact_all_after_marker(&redacted, marker);
    }
    truncate_utf8(&redacted, MAX_FAILURE_MESSAGE_BYTES)
}

fn redact_all_after_marker(value: &str, marker: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let lower_tail = value[cursor..].to_ascii_lowercase();
        let Some(relative_start) = lower_tail.find(marker) else {
            output.push_str(&value[cursor..]);
            break;
        };
        let start = cursor + relative_start;
        let mut value_start = start + marker.len();
        while value_start < value.len()
            && value[value_start..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            value_start += value[value_start..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        }
        let value_end = value[value_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ',' | ';' | '}' | ']' | '"')
            })
            .map_or(value.len(), |offset| value_start + offset);
        output.push_str(&value[cursor..value_start]);
        output.push_str("<redacted>");
        cursor = value_end;
    }
    output
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn encode_failure_response(error: &ProviderFailure, request_id: String) -> Vec<u8> {
    let response = FailureResponse {
        schema_version: PROTOCOL_VERSION,
        request_id: truncate_utf8(&request_id, 128),
        ok: false,
        error: ProviderFailureBody {
            code: error.code,
            message: bounded_redacted_message(&error.message),
            retryable: error.retryable,
        },
    };
    match serde_json::to_vec(&response) {
        Ok(bytes) if bytes.len() <= MAX_FAILURE_RESPONSE_BYTES => bytes,
        _ => serde_json::to_vec(&FailureResponse {
            schema_version: PROTOCOL_VERSION,
            request_id: String::new(),
            ok: false,
            error: ProviderFailureBody {
                code: "provider_failed",
                message: "provider failure response exceeded its bounded envelope".to_owned(),
                retryable: false,
            },
        })
        .unwrap_or_else(|_| b"{\"schemaVersion\":1,\"requestId\":\"\",\"ok\":false,\"error\":{\"code\":\"provider_failed\",\"message\":\"provider failure\",\"retryable\":false}}".to_vec()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (raw, request_id) = read_request();
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
        Err(error) => Ok(encode_failure_response(&error, request_id)),
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

fn reflected_request_id_from_bytes(bytes: &[u8]) -> String {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|value| {
            value
                .get("requestId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|request_id| valid_request_id(request_id))
        .unwrap_or_default()
}

fn valid_request_id(request_id: &str) -> bool {
    !request_id.trim().is_empty() && request_id.len() <= 128 && !request_id.contains('\0')
}

fn read_request() -> (ProviderResult<ProviderRequest>, String) {
    let mut bytes = Vec::new();
    if let Err(error) = io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return (
            Err(failure(
                "invalid_request",
                &format!("cannot read request: {error}"),
                false,
            )),
            String::new(),
        );
    }
    if bytes.len() > MAX_REQUEST_BYTES {
        return (
            Err(failure(
                "resource_limit",
                "request exceeds the bounded JSONL frame limit",
                false,
            )),
            String::new(),
        );
    }
    match serde_json::from_slice::<ProviderRequest>(&bytes) {
        Ok(request) => {
            let request_id = if valid_request_id(&request.request_id) {
                request.request_id.clone()
            } else {
                String::new()
            };
            (Ok(request), request_id)
        }
        Err(error) => (
            Err(failure(
                "invalid_request",
                &format!("request is not strict JSON: {error}"),
                false,
            )),
            reflected_request_id_from_bytes(&bytes),
        ),
    }
}

fn validate_request(request: ProviderRequest) -> ProviderResult<ProviderRequest> {
    if request.schema_version != 1
        || request.request_id.trim().is_empty()
        || request.request_id.contains('\0')
    {
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
    } else if request.image_options.is_some() {
        return Err(failure(
            "invalid_request",
            "imageOptions is only valid for image_generate",
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
        validate_bounded_text(&tool.name, "tool name", MAX_TOOL_NAME_BYTES)?;
        validate_text(&tool.description, "tool description")?;
        if !tool.strict {
            return Err(failure(
                "invalid_request",
                "tools must use strict object schemas",
                false,
            ));
        }
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
    if request.operation == ProviderOperation::ModelTurn {
        let output_schema = model_output_schema(&request)?;
        let schema_bytes = serde_json::to_vec(&output_schema).map_err(|error| {
            failure(
                "invalid_request",
                &format!("could not encode model output schema: {error}"),
                false,
            )
        })?;
        if schema_bytes.len() > 64 * 1024 {
            return Err(failure(
                "resource_limit",
                "model output schema exceeds the 64 KiB native bound",
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
    validate_bounded_text(&call.id, "tool call id", MAX_TOOL_CALL_ID_BYTES)?;
    validate_bounded_text(&call.name, "tool call name", MAX_TOOL_NAME_BYTES)?;
    if !call.arguments.is_object() {
        return Err(failure(
            "invalid_request",
            "tool call arguments must be a JSON object",
            false,
        ));
    }
    let argument_bytes = serde_json::to_vec(&call.arguments)
        .map_err(|error| failure("invalid_request", &error.to_string(), false))?
        .len();
    if argument_bytes > MAX_TOOL_ARGUMENT_BYTES {
        return Err(failure(
            "resource_limit",
            "tool call arguments exceed the bounded byte limit",
            false,
        ));
    }
    Ok(())
}

fn validate_bounded_text(value: &str, label: &str, maximum: usize) -> ProviderResult<()> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(failure(
            "invalid_request",
            &format!("{label} must be a bounded non-empty UTF-8 value"),
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
            &format!("configured runtime verification failed: {error}"),
            false,
        )
    })?;
    let enable_images = request.operation == ProviderOperation::ImageGenerate;
    let codex = connect(
        runtime,
        enable_images,
        &config.codex_home,
        Duration::from_millis(request.timeout_ms),
    )
    .await?;
    let operation = match request.operation {
        ProviderOperation::ModelTurn => run_model_turn(&codex, &config, &request).await,
        ProviderOperation::ImageGenerate => run_image_generation(&codex, &request).await,
    };
    let shutdown = codex
        .shutdown()
        .await
        .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()));
    match (operation, shutdown) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(operation_error), Err(shutdown_error)) => Err(failure(
            operation_error.code,
            &format!(
                "{}; client shutdown also failed: {}",
                operation_error.message, shutdown_error.message
            ),
            false,
        )),
    }
}

async fn connect(
    runtime: RuntimePackage,
    enable_images: bool,
    codex_home: &std::path::Path,
    request_timeout: Duration,
) -> ProviderResult<Codex> {
    Codex::connect(
        CodexConfig::new(runtime)
            .with_codex_home(codex_home)
            .map_err(|error| failure("invalid_configuration", &error.to_string(), false))?
            .with_image_generation(enable_images)
            .with_request_timeout(request_timeout)
            .with_max_frame_bytes(if enable_images {
                MAX_IMAGE_FRAME_BYTES
            } else {
                MAX_FRAME_BYTES
            })
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
    let output_schema = model_output_schema(request)?;
    let mut options = SessionOptions::read_only(&config.workspace)
        .with_model(&config.model)
        .with_reasoning(map_reasoning(request.reasoning))
        .with_turn_timeout(Duration::from_millis(request.timeout_ms))
        .with_maximum_output_bytes(model_session_bytes(request.maximum_response_bytes))
        .with_output_schema(output_schema)
        .text_only();
    if let Some(base) = rendered.base_instructions {
        options = options.with_base_instructions(base);
    }
    if let Some(developer) = rendered.developer_instructions {
        options = options.with_developer_instructions(developer);
    }
    let session = codex
        .session(options)
        .await
        .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
    let verification = async {
        let result = session
            .start(rendered.prompt)
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?
            .wait()
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
        if result.status != TurnStatus::Completed || !result.image_generations.is_empty() {
            return Err(failure(
                "provider_failed",
                "model turn did not complete as text-only output",
                false,
            ));
        }
        let audit = session
            .audit_turn(&result.turn_id)
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
        if !audit.commands.is_empty()
            || !audit.file_changes.is_empty()
            || !audit.image_generations.is_empty()
            || audit.other_item_types.iter().any(|item_type| {
                !matches!(
                    item_type.as_str(),
                    "userMessage" | "agentMessage" | "reasoning"
                )
            })
        {
            return Err(failure(
                "provider_failed",
                "model turn durable audit recorded an unsupported effect item",
                false,
            ));
        }
        let (text, tool_calls) = if request.tools.is_empty() {
            let body: ModelTextResponseBody =
                serde_json::from_str(result.text.trim()).map_err(|error| {
                    failure(
                        "invalid_provider_output",
                        &format!(
                            "model output did not match the strict no-tool response schema: {error}"
                        ),
                        false,
                    )
                })?;
            (body.text, Vec::new())
        } else {
            let body: ModelToolResponseBody =
                serde_json::from_str(result.text.trim()).map_err(|error| {
                    failure(
                        "invalid_provider_output",
                        &format!(
                            "model output did not match the strict tool response schema: {error}"
                        ),
                        false,
                    )
                })?;
            (
                body.text,
                flatten_native_tool_calls(body.tool_calls, &request.tools)?,
            )
        };
        if text.len() > request.maximum_response_bytes || tool_calls.len() > MAX_TOOL_CALLS {
            return Err(failure(
                "resource_limit",
                "structured model response exceeded the configured bound",
                false,
            ));
        }
        validate_returned_tools(&request.tools, &tool_calls)?;
        Ok(Response::Model(ModelResponse {
            schema_version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            operation: ProviderOperation::ModelTurn,
            text,
            tool_calls,
            usage: result.usage.map(|usage| ProviderUsage {
                input_tokens: Some(usage.input_tokens),
                output_tokens: Some(usage.output_tokens),
                total_tokens: Some(usage.total_tokens),
            }),
        }))
    }
    .await;
    let close = session.close().await;
    match (verification, close) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(classify_vergerail_error(&error.to_string(), error.kind())),
        (Err(operation_error), Err(close_error)) => Err(failure(
            operation_error.code,
            &format!(
                "{}; model session close also failed: {}",
                operation_error.message, close_error
            ),
            false,
        )),
    }
}

fn model_session_bytes(maximum_text_bytes: usize) -> usize {
    JSON_STRING_MAX_EXPANSION
        .saturating_mul(
            maximum_text_bytes
                .saturating_add(MAX_TOOL_ARGUMENTS_BYTES)
                .saturating_add(
                    MAX_TOOL_CALLS
                        .saturating_mul(MAX_TOOL_CALL_ID_BYTES.saturating_add(MAX_TOOL_NAME_BYTES)),
                ),
        )
        .saturating_add(MAX_TOOL_CALLS.saturating_mul(MODEL_TOOL_CALL_WRAPPER_BYTES))
        .saturating_add(MODEL_RESPONSE_HEADROOM_BYTES)
        .min(MAX_MODEL_SESSION_BYTES)
}

async fn run_image_generation(
    codex: &Codex,
    request: &ProviderRequest,
) -> ProviderResult<Response> {
    let prompt = request
        .prompt
        .as_deref()
        .ok_or_else(|| failure("invalid_request", "image_generate requires prompt", false))?;
    let options = request.image_options.as_ref();
    let direct = DirectImageRequest {
        model: "gpt-image-1".to_owned(),
        prompt: prompt.to_owned(),
        background: options
            .and_then(|options| options.background)
            .map(direct_background)
            .unwrap_or(ImageBackground::Auto),
        size: options
            .and_then(|options| options.size)
            .map(direct_size)
            .unwrap_or(ImageSize::Auto),
        quality: options
            .and_then(|options| options.quality)
            .map(direct_quality)
            .unwrap_or(ImageQuality::Auto),
    };
    let generated = codex
        .generate_image(direct)
        .await
        .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
    let mut provider_image =
        provider_image_from_base64(generated.base64(), request.maximum_response_bytes)?;
    if (provider_image.width, provider_image.height) != (generated.width(), generated.height()) {
        return Err(failure(
            "invalid_provider_output",
            "direct image response dimensions did not match the decoded PNG",
            false,
        ));
    }
    provider_image.transparent_background = generated.transparent_background();
    Ok(Response::Image(ImageResponse {
        schema_version: PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        operation: ProviderOperation::ImageGenerate,
        image: provider_image,
    }))
}

fn direct_background(value: ImageBackgroundOption) -> ImageBackground {
    match value {
        ImageBackgroundOption::Auto => ImageBackground::Auto,
        ImageBackgroundOption::Transparent => ImageBackground::Transparent,
        ImageBackgroundOption::Opaque => ImageBackground::Opaque,
    }
}

fn direct_size(value: ImageSizeOption) -> ImageSize {
    match value {
        ImageSizeOption::Auto => ImageSize::Auto,
        ImageSizeOption::Square => ImageSize::Square,
        ImageSizeOption::Landscape => ImageSize::Landscape,
        ImageSizeOption::Portrait => ImageSize::Portrait,
    }
}

fn direct_quality(value: ImageQualityOption) -> ImageQuality {
    match value {
        ImageQualityOption::Auto => ImageQuality::Auto,
        ImageQualityOption::Low => ImageQuality::Low,
        ImageQualityOption::Medium => ImageQuality::Medium,
        ImageQualityOption::High => ImageQuality::High,
    }
}

fn provider_image_from_base64(
    result_base64: &str,
    maximum_response_bytes: usize,
) -> ProviderResult<ProviderImage> {
    if result_base64.len() > MAX_IMAGE_BASE64_BYTES {
        return Err(failure(
            "resource_limit",
            "encoded image exceeds the bounded output frame limit",
            false,
        ));
    }
    let bytes = BASE64_STANDARD.decode(result_base64).map_err(|error| {
        failure(
            "invalid_provider_output",
            &format!("image result was not valid base64: {error}"),
            false,
        )
    })?;
    if bytes.len() > MAX_IMAGE_BYTES || bytes.len() > maximum_response_bytes {
        return Err(failure(
            "resource_limit",
            "decoded image exceeds the bounded output limit",
            false,
        ));
    }
    let (width, height) = validate_png(&bytes)?;
    Ok(ProviderImage {
        media_type: "image/png",
        base64: BASE64_STANDARD.encode(&bytes),
        byte_length: bytes.len(),
        width,
        height,
        transparent_background: None,
    })
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
            "Process the provider-neutral conversation below. Reply with exactly one compact JSON object matching the native output schema, with no markdown or surrounding text. When tools are advertised, toolCalls is an object containing every advertised tool name as a key; each value is an array of {{id, arguments}} calls, and an unused tool must use an empty array. With no tools, return only the text property. Tool arguments must match the advertised strict schemas.\n{encoded}"
        ),
        base_instructions: (!base.is_empty()).then(|| base.join("\n\n")),
        developer_instructions: (!developer.is_empty()).then(|| developer.join("\n\n")),
    })
}

fn model_output_schema(request: &ProviderRequest) -> ProviderResult<Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "text".to_owned(),
        serde_json::json!({"type": "string", "maxLength": MAX_TEXT_BYTES}),
    );
    let mut required = vec![Value::String("text".to_owned())];
    if !request.tools.is_empty() {
        let mut tool_properties = serde_json::Map::new();
        let mut tool_names = Vec::with_capacity(request.tools.len());
        for tool in &request.tools {
            tool_names.push(Value::String(tool.name.clone()));
            tool_properties.insert(
                tool.name.clone(),
                serde_json::json!({
                    "type": "array",
                    "maxItems": MAX_TOOL_CALLS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id", "arguments"],
                        "properties": {
                            "id": {"type": "string", "maxLength": MAX_TOOL_CALL_ID_BYTES},
                            "arguments": tool.input_schema
                        }
                    }
                }),
            );
        }
        properties.insert(
            "toolCalls".to_owned(),
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": tool_names,
                "properties": tool_properties
            }),
        );
        required.push(Value::String("toolCalls".to_owned()));
    }
    let mut schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties
    });
    close_native_object_schemas(&mut schema)?;
    Ok(schema)
}

/// Native structured-output schemas require every object schema to explicitly
/// close additional properties and list every property as required. Tool
/// schemas come from the provider-neutral request, so normalize those rules
/// recursively before the schema crosses the native app-server boundary. An
/// explicitly open object schema is rejected rather than silently changing the
/// advertised tool contract; an omitted optional property becomes a required
/// nullable property, which is the native strict-schema representation.
fn close_native_object_schemas(schema: &mut Value) -> ProviderResult<()> {
    match schema {
        Value::Object(object) => {
            let object_schema = match object.get("type") {
                Some(Value::String(kind)) => kind == "object",
                Some(Value::Array(types)) => {
                    types.iter().any(|value| value.as_str() == Some("object"))
                }
                _ => false,
            } || object.contains_key("properties")
                || object.contains_key("required");
            if object_schema {
                match object.get("additionalProperties") {
                    None | Some(Value::Bool(false)) => {}
                    Some(_) => {
                        return Err(failure(
                            "invalid_request",
                            "native strict object schemas require additionalProperties to be false",
                            false,
                        ));
                    }
                }
                object.insert("additionalProperties".to_owned(), Value::Bool(false));

                let property_names = object
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let existing_required = object
                    .get("required")
                    .map(|required| {
                        required
                            .as_array()
                            .ok_or_else(|| {
                                failure(
                                    "invalid_request",
                                    "native strict object schemas require required to be an array",
                                    false,
                                )
                            })?
                            .iter()
                            .map(|value| {
                                value.as_str().map(str::to_owned).ok_or_else(|| {
                                    failure(
                                        "invalid_request",
                                        "native strict object schema required entries must be strings",
                                        false,
                                    )
                                })
                            })
                            .collect::<ProviderResult<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                if existing_required
                    .iter()
                    .any(|name| !property_names.iter().any(|property| property == name))
                {
                    return Err(failure(
                        "invalid_request",
                        "native strict object schema required contains a property that is not declared",
                        false,
                    ));
                }
                if let Some(properties) =
                    object.get_mut("properties").and_then(Value::as_object_mut)
                {
                    for name in &property_names {
                        if !existing_required.iter().any(|required| required == name)
                            && let Some(property) = properties.get_mut(name)
                        {
                            make_native_property_nullable(property);
                        }
                    }
                }
                object.insert(
                    "required".to_owned(),
                    Value::Array(property_names.into_iter().map(Value::String).collect()),
                );
            }
            for child in object.values_mut() {
                close_native_object_schemas(child)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                close_native_object_schemas(item)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

fn make_native_property_nullable(schema: &mut Value) {
    if let Value::Object(object) = schema {
        if let Some(type_value) = object.get("type")
            && (type_value.as_str() == Some("null")
                || type_value
                    .as_array()
                    .is_some_and(|types| types.iter().any(|value| value.as_str() == Some("null"))))
        {
            return;
        }
        if object
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|variants| {
                variants.iter().any(|variant| {
                    variant
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "null")
                })
            })
        {
            return;
        }
        if let Some(type_value) = object.get_mut("type") {
            match type_value {
                Value::String(kind) => {
                    let kind = std::mem::take(kind);
                    *type_value =
                        Value::Array(vec![Value::String(kind), Value::String("null".to_owned())]);
                    return;
                }
                Value::Array(types) => {
                    types.push(Value::String("null".to_owned()));
                    return;
                }
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => {}
            }
        }
    }
    let original = schema.clone();
    *schema = serde_json::json!({
        "anyOf": [original, {"type": "null"}]
    });
}

fn flatten_native_tool_calls(
    mut grouped: BTreeMap<String, Vec<NativeToolCall>>,
    tools: &[ProviderTool],
) -> ProviderResult<Vec<ProviderToolCall>> {
    let mut flattened = Vec::new();
    for tool in tools {
        let calls = grouped.remove(&tool.name).ok_or_else(|| {
            failure(
                "invalid_provider_output",
                "native toolCalls omitted an advertised tool key",
                false,
            )
        })?;
        for call in calls {
            flattened.push(ProviderToolCall {
                id: call.id,
                name: tool.name.clone(),
                arguments: call.arguments,
            });
            if flattened.len() > MAX_TOOL_CALLS {
                return Err(failure(
                    "resource_limit",
                    "aggregate native tool calls exceed the bounded provider limit",
                    false,
                ));
            }
        }
    }
    if !grouped.is_empty() {
        return Err(failure(
            "invalid_provider_output",
            "native toolCalls contained an unadvertised tool key",
            false,
        ));
    }
    Ok(flattened)
}

fn validate_returned_tools(
    tools: &[ProviderTool],
    calls: &[ProviderToolCall],
) -> ProviderResult<()> {
    if calls.len() > MAX_TOOL_CALLS {
        return Err(failure(
            "resource_limit",
            "provider returned too many tool calls",
            false,
        ));
    }
    let advertised = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut ids = std::collections::BTreeSet::new();
    let mut aggregate_argument_bytes = 0_usize;
    for call in calls {
        validate_tool_call(call)?;
        let argument_bytes = serde_json::to_vec(&call.arguments)
            .map_err(|error| failure("invalid_provider_output", &error.to_string(), false))?
            .len();
        aggregate_argument_bytes = aggregate_argument_bytes
            .checked_add(argument_bytes)
            .ok_or_else(|| {
                failure(
                    "resource_limit",
                    "aggregate tool-call argument size overflowed",
                    false,
                )
            })?;
        if aggregate_argument_bytes > MAX_TOOL_ARGUMENTS_BYTES {
            return Err(failure(
                "resource_limit",
                "aggregate tool-call arguments exceed the bounded byte limit",
                false,
            ));
        }
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
        ProviderReasoning::XHigh => ReasoningEffort::XHigh,
        ProviderReasoning::Max => ReasoningEffort::Max,
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
    failure(code, message, false)
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
            image_options: None,
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
    fn image_options_are_strictly_typed_and_only_valid_for_image_generation() {
        let mut image = request(ProviderOperation::ImageGenerate);
        image.messages.clear();
        image.prompt = Some("make a game sprite".to_owned());
        image.image_options = Some(ImageGenerationOptions {
            background: Some(ImageBackgroundOption::Transparent),
            size: Some(ImageSizeOption::Portrait),
            quality: Some(ImageQualityOption::High),
        });
        validate_request(image).expect("typed image options");

        let mut model = request(ProviderOperation::ModelTurn);
        model.image_options = Some(ImageGenerationOptions {
            background: Some(ImageBackgroundOption::Opaque),
            size: None,
            quality: None,
        });
        assert_eq!(
            validate_request(model)
                .expect_err("image options must not cross the model-turn contract")
                .code,
            "invalid_request"
        );

        assert!(
            serde_json::from_value::<ImageGenerationOptions>(json!({
                "background": "transparent",
                "size": "2048x2048",
                "quality": "high"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ImageGenerationOptions>(json!({
                "background": "transparent",
                "size": "1024x1024",
                "quality": "high",
                "unknown": true
            }))
            .is_err()
        );
    }

    #[test]
    fn all_reasoning_wire_values_map_exactly_and_omission_is_rejected() {
        for (wire, expected) in [
            ("off", ReasoningEffort::None),
            ("low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::XHigh),
            ("max", ReasoningEffort::Max),
        ] {
            let parsed: ProviderReasoning =
                serde_json::from_value(json!(wire)).expect("supported reasoning value");
            assert_eq!(map_reasoning(parsed), expected);
        }
        assert!(serde_json::from_value::<ProviderReasoning>(json!("x_high")).is_err());
        let mut omitted = serde_json::to_value(request(ProviderOperation::ModelTurn))
            .expect("provider request JSON");
        omitted
            .as_object_mut()
            .expect("request object")
            .remove("reasoning");
        assert!(serde_json::from_value::<ProviderRequest>(omitted).is_err());
    }

    #[test]
    fn native_output_schema_propagates_each_advertised_tool_schema() {
        let mut value = request(ProviderOperation::ModelTurn);
        value.tools.push(ProviderTool {
            name: "inspect_asset".to_owned(),
            description: "Inspect one image".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["artifactId"],
                "properties": {"artifactId": {"type": "string"}}
            }),
            strict: true,
        });
        let schema = model_output_schema(&value).expect("native schema");
        assert_eq!(
            schema
                .pointer("/properties/toolCalls/properties/inspect_asset/items/properties/id/type"),
            Some(&json!("string"))
        );
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/inspect_asset/items/properties/arguments/properties/artifactId/type"
            ),
            Some(&json!("string"))
        );
        assert_eq!(
            schema.pointer("/properties/toolCalls/required"),
            Some(&json!(["inspect_asset"]))
        );
        assert!(
            !serde_json::to_string(&schema)
                .expect("schema JSON")
                .contains("oneOf")
        );
        validate_request(value).expect("strict bounded tool schema");
    }

    #[test]
    fn native_output_schema_closes_empty_items_and_nested_object_arguments() {
        let empty =
            model_output_schema(&request(ProviderOperation::ModelTurn)).expect("empty tool schema");
        assert_native_object_schemas(&empty);
        assert_eq!(empty.pointer("/properties/toolCalls"), None);
        assert_eq!(empty.pointer("/required"), Some(&json!(["text"])));

        let mut value = request(ProviderOperation::ModelTurn);
        value.tools.push(ProviderTool {
            name: "inspect_asset".to_owned(),
            description: "Inspect one image".to_owned(),
            input_schema: json!({
                "type": "object",
                "required": ["options"],
                "properties": {
                    "options": {
                        "type": "object",
                        "required": ["artifactId"],
                        "properties": {
                            "artifactId": {"type": "string"},
                            "format": {"type": "string"}
                        }
                    }
                }
            }),
            strict: true,
        });
        let schema = model_output_schema(&value).expect("nested native schema");
        assert_native_object_schemas(&schema);
        for pointer in [
            "/properties/toolCalls/additionalProperties",
            "/properties/toolCalls/properties/inspect_asset/items/additionalProperties",
            "/properties/toolCalls/properties/inspect_asset/items/properties/arguments/additionalProperties",
            "/properties/toolCalls/properties/inspect_asset/items/properties/arguments/properties/options/additionalProperties",
        ] {
            assert_eq!(schema.pointer(pointer), Some(&json!(false)), "{pointer}");
        }
        assert_eq!(
            schema.pointer("/properties/toolCalls/properties/inspect_asset/items/required"),
            Some(&json!(["arguments", "id"]))
        );
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/inspect_asset/items/properties/arguments/required"
            ),
            Some(&json!(["options"]))
        );
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/inspect_asset/items/properties/arguments/properties/options/required"
            ),
            Some(&json!(["artifactId", "format"]))
        );
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/inspect_asset/items/properties/arguments/properties/options/properties/format/type"
            ),
            Some(&json!(["string", "null"]))
        );
        assert_eq!(
            schema.pointer("/required"),
            Some(&json!(["text", "toolCalls"]))
        );
        validate_request(value).expect("nested strict bounded tool schema");
    }

    fn assert_native_object_schemas(schema: &Value) {
        match schema {
            Value::Object(object) => {
                let object_schema = match object.get("type") {
                    Some(Value::String(kind)) => kind == "object",
                    Some(Value::Array(types)) => {
                        types.iter().any(|value| value.as_str() == Some("object"))
                    }
                    _ => false,
                } || object.contains_key("properties")
                    || object.contains_key("required");
                if object_schema {
                    assert_eq!(object.get("additionalProperties"), Some(&json!(false)));
                    let property_names = object
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|properties| properties.keys().collect::<Vec<_>>())
                        .unwrap_or_default();
                    let required_names = object
                        .get("required")
                        .and_then(Value::as_array)
                        .expect("strict object required array")
                        .iter()
                        .map(|value| value.as_str().expect("required property name"))
                        .collect::<Vec<_>>();
                    let mut property_names = property_names;
                    let mut required_names = required_names;
                    property_names.sort_unstable();
                    required_names.sort_unstable();
                    assert_eq!(property_names, required_names);
                }
                for child in object.values() {
                    assert_native_object_schemas(child);
                }
            }
            Value::Array(items) => {
                for item in items {
                    assert_native_object_schemas(item);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    #[test]
    fn native_output_schema_rejects_open_object_argument_schemas() {
        let mut value = request(ProviderOperation::ModelTurn);
        value.tools.push(ProviderTool {
            name: "open_tool".to_owned(),
            description: "Open schema is not native-strict".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": true
            }),
            strict: true,
        });
        assert_eq!(
            validate_request(value)
                .expect_err("an open object schema must not cross the native boundary")
                .code,
            "invalid_request"
        );
    }

    #[test]
    fn returned_tools_enforce_identity_object_and_aggregate_bounds() {
        let tools = vec![ProviderTool {
            name: "inspect_asset".to_owned(),
            description: "Inspect one image".to_owned(),
            input_schema: json!({"type": "object"}),
            strict: true,
        }];
        let valid = ProviderToolCall {
            id: "call-1".to_owned(),
            name: "inspect_asset".to_owned(),
            arguments: json!({"artifactId": "asset-1"}),
        };
        validate_returned_tools(&tools, std::slice::from_ref(&valid))
            .expect("advertised bounded call");

        let mut unknown = valid.clone();
        unknown.name = "other".to_owned();
        assert_eq!(
            validate_returned_tools(&tools, &[unknown])
                .expect_err("unadvertised tool")
                .code,
            "invalid_provider_output"
        );

        let oversized = ProviderToolCall {
            arguments: json!({"value": "x".repeat(MAX_TOOL_ARGUMENT_BYTES)}),
            ..valid
        };
        assert_eq!(
            validate_returned_tools(&tools, &[oversized])
                .expect_err("oversized arguments")
                .code,
            "resource_limit"
        );
    }

    #[test]
    fn strict_model_body_rejects_unknown_fields() {
        assert!(
            serde_json::from_value::<ModelTextResponseBody>(json!({
                "text": "ok",
                "toolCalls": {}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ModelToolResponseBody>(json!({
                "text": "ok",
                "toolCalls": {"inspect_asset": []},
                "usage": null
            }))
            .is_err()
        );
    }

    #[test]
    fn native_tool_calls_flatten_in_advertised_order_and_require_all_keys() {
        let tools = vec![
            ProviderTool {
                name: "z_tool".to_owned(),
                description: "Z".to_owned(),
                input_schema: json!({"type": "object"}),
                strict: true,
            },
            ProviderTool {
                name: "a_tool".to_owned(),
                description: "A".to_owned(),
                input_schema: json!({"type": "object"}),
                strict: true,
            },
        ];
        let grouped = BTreeMap::from([
            (
                "a_tool".to_owned(),
                vec![NativeToolCall {
                    id: "a-1".to_owned(),
                    arguments: json!({}),
                }],
            ),
            (
                "z_tool".to_owned(),
                vec![NativeToolCall {
                    id: "z-1".to_owned(),
                    arguments: json!({}),
                }],
            ),
        ]);
        let flattened = flatten_native_tool_calls(grouped, &tools).expect("flattened calls");
        assert_eq!(
            flattened
                .iter()
                .map(|call| (call.name.as_str(), call.id.as_str()))
                .collect::<Vec<_>>(),
            [("z_tool", "z-1"), ("a_tool", "a-1")]
        );

        let missing = BTreeMap::from([("z_tool".to_owned(), Vec::new())]);
        assert_eq!(
            flatten_native_tool_calls(missing, &tools)
                .expect_err("missing advertised key")
                .code,
            "invalid_provider_output"
        );
    }

    #[test]
    fn failure_messages_are_redacted_bounded_and_encoded_within_frame() {
        let message = format!(
            "Authorization: Bearer secret-token; details={} access_token=another-secret",
            "x".repeat(MAX_FAILURE_MESSAGE_BYTES * 4)
        );
        let error = failure("provider_failed", &message, false);
        assert!(error.message.len() <= MAX_FAILURE_MESSAGE_BYTES);
        assert!(!error.message.contains("secret-token"));
        assert!(!error.message.contains("another-secret"));
        assert!(
            encode_failure_response(&error, "request-1".to_owned()).len()
                <= MAX_FAILURE_RESPONSE_BYTES
        );
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
    fn image_payload_requires_bounded_png_and_reports_exact_dimensions() {
        assert_eq!(
            provider_image_from_base64("not-base64", MAX_IMAGE_BYTES)
                .expect_err("base64 failure")
                .code,
            "invalid_provider_output"
        );
        assert_eq!(
            provider_image_from_base64(&BASE64_STANDARD.encode(b"not-png"), MAX_IMAGE_BYTES)
                .expect_err("PNG failure")
                .code,
            "invalid_provider_output"
        );

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2u32.to_be_bytes());
        png.extend_from_slice(&3u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&0u32.to_be_bytes());
        let encoded = BASE64_STANDARD.encode(&png);
        let image = provider_image_from_base64(&encoded, png.len()).expect("bounded PNG");
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.byte_length, png.len());
        assert_eq!(
            provider_image_from_base64(&encoded, png.len() - 1)
                .expect_err("caller byte cap")
                .code,
            "resource_limit"
        );
    }

    #[test]
    fn model_session_bound_covers_json_escaping_and_stays_bounded() {
        assert!(model_session_bytes(MAX_TEXT_BYTES) > MAX_TEXT_BYTES);
        assert!(model_session_bytes(MAX_TEXT_BYTES) <= MAX_MODEL_SESSION_BYTES);
        assert!(model_session_bytes(1) >= MODEL_RESPONSE_HEADROOM_BYTES);
    }

    #[test]
    fn image_frame_is_distinct_and_contains_the_bounded_encoded_raster() {
        assert_eq!(MAX_IMAGE_FRAME_BYTES, 16 * 1024 * 1024);
        assert_ne!(MAX_IMAGE_FRAME_BYTES, MAX_FRAME_BYTES);
    }

    #[test]
    fn protocol_schema_is_versioned() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
