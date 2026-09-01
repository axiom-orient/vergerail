//! Versioned, bounded JSONL provider transport for UpAgent.
//!
//! The process owns no credentials. It accepts a provider-neutral turn or an
//! image-generation request on stdin, runs one read-only/text-only Vergerail
//! operation against the standard Codex account, and emits one strict JSON
//! response on stdout. A caller that cannot observe the
//! result must resolve the outcome; this binary never retries a request.

#![forbid(unsafe_code)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Read as _, Write};
use std::path::{Component, PathBuf};
use std::time::Duration;
use std::time::Instant;
use tokio::time::timeout;
use vergerail::{
    Codex, CodexConfig, DirectImageRequest, Event, ImageBackground, ImageDetail, ImageQuality,
    ImageSize, ReasoningEffort, Run, RunResult, RuntimePackage, SessionOptions, TurnInput,
    TurnStatus, validate_png_dimensions,
};

const PROTOCOL_VERSION: u32 = 2;
const MAX_REQUEST_BYTES: usize = CodexConfig::MAX_FRAME_BYTES;
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
const MAX_MODEL_SESSION_BYTES: usize = CodexConfig::MAX_FRAME_BYTES;
const MAX_FAILURE_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_FAILURE_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const STAGING_ROOT_PREFIX: &str = "vergerail-upagent-request-";
// This cleanup budget begins only after the user-visible provider deadline.
// It is deliberately separate from timeoutMs and bounds the final reap.
const PROVIDER_TEARDOWN_BUDGET: Duration = Duration::from_secs(2);
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
    observations: Vec<ProviderObservation>,
    /// Exact request-specific root created and owned by UpAgent for staged
    /// observation files. It is never copied into the model prompt.
    #[serde(default)]
    staging_root: Option<PathBuf>,
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
    content_parts: Vec<ProviderContentPart>,
    #[serde(default)]
    tool_calls: Vec<ProviderToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ProviderContentPart {
    Text { text: String },
    ImageObservation { image: ProviderImageObservation },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderImageObservation {
    artifact: ProviderArtifactRef,
    role: ProviderImageRole,
    #[serde(default)]
    detail: ProviderImageDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    caption: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderArtifactRef {
    id: String,
    sha256: String,
    media_type: String,
    byte_length: u64,
    relative_path: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProviderImageRole {
    Full,
    Crop,
    Mask,
    AlphaCheckerboard,
    Diff,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProviderImageDetail {
    Low,
    #[default]
    Auto,
    High,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderObservation {
    message_index: usize,
    part_index: usize,
    media_type: String,
    sha256: String,
    width: u32,
    height: u32,
    role: ProviderImageRole,
    base64: String,
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
    media_type: String,
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
    model: String,
    workspace: PathBuf,
}

impl ProviderConfig {
    fn from_environment() -> ProviderResult<Self> {
        let runtime_package = required_path("VERGERAIL_CODEX_PACKAGE")?;
        let model = required_string("VERGERAIL_MODEL")?;
        let workspace = required_path("VERGERAIL_WORKSPACE")?;
        if !absolute_clean_path(&runtime_package) || !absolute_clean_path(&workspace) {
            return Err(failure(
                "invalid_configuration",
                "runtime package and workspace must be absolute paths without parent traversal",
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

fn validate_staging_root(path: &std::path::Path) -> ProviderResult<()> {
    if !absolute_clean_path(path)
        || path.parent() != Some(env::temp_dir().as_path())
        || !path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with(STAGING_ROOT_PREFIX))
    {
        return Err(failure(
            "invalid_request",
            "stagingRoot must be an exact UpAgent request directory under the system temporary directory",
            false,
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        failure(
            "invalid_request",
            &format!("stagingRoot is not an accessible request directory: {error}"),
            false,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(failure(
            "invalid_request",
            "stagingRoot must be a non-symlink directory",
            false,
        ));
    }
    let mut entries = fs::read_dir(path).map_err(|error| {
        failure(
            "invalid_request",
            &format!("stagingRoot cannot be inspected safely: {error}"),
            false,
        )
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|error| {
            failure(
                "invalid_request",
                &format!("stagingRoot cannot be inspected safely: {error}"),
                false,
            )
        })?
        .is_some()
    {
        return Err(failure(
            "invalid_request",
            "stagingRoot must be an empty request directory owned by UpAgent",
            false,
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            return Err(failure(
                "invalid_request",
                "stagingRoot must have private 0700 permissions",
                false,
            ));
        }
    }
    Ok(())
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
        .unwrap_or_else(|_| b"{\"schemaVersion\":2,\"requestId\":\"\",\"ok\":false,\"error\":{\"code\":\"provider_failed\",\"message\":\"provider failure\",\"retryable\":false}}".to_vec()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Some(exit_code) = runtime_verification_helper_exit_code() {
        std::process::exit(exit_code);
    }
    let (raw, request_id) = read_request();
    let parsed = raw.and_then(validate_request);
    let result = match parsed {
        Ok(request) => match ProviderConfig::from_environment() {
            Ok(config) => async_run_request(request, config).await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    let bytes = match result {
        Ok(response) => match encode_success_response(response) {
            Ok(bytes) => bytes,
            Err(error) => encode_failure_response(&error, request_id),
        },
        Err(error) => encode_failure_response(&error, request_id),
    };
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    if write_response(&mut stdout, &bytes).is_err() {
        std::process::exit(1);
    }
}

fn encode_success_response(response: Response) -> ProviderResult<Vec<u8>> {
    let (bytes, maximum) = match response {
        Response::Model(response) => (
            serde_json::to_vec(&response).map_err(|error| {
                failure(
                    "provider_failed",
                    &format!("model response could not be encoded: {error}"),
                    false,
                )
            })?,
            CodexConfig::MAX_FRAME_BYTES,
        ),
        Response::Image(response) => (
            serde_json::to_vec(&response).map_err(|error| {
                failure(
                    "provider_failed",
                    &format!("image response could not be encoded: {error}"),
                    false,
                )
            })?,
            MAX_IMAGE_FRAME_BYTES,
        ),
    };
    if bytes.len() > maximum {
        return Err(failure(
            "resource_limit",
            "provider response exceeds its bounded output frame",
            false,
        ));
    }
    Ok(bytes)
}

fn runtime_verification_helper_exit_code() -> Option<i32> {
    let mut arguments = env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(OsStr::new("--vergerail-runtime-verify")) {
        return None;
    }
    let Some(root) = arguments.next() else {
        eprintln!("runtime verification helper requires a package root");
        return Some(64);
    };
    if arguments.next().is_some() {
        eprintln!("runtime verification helper received unexpected arguments");
        return Some(64);
    }
    let result =
        RuntimePackage::pinned(PathBuf::from(root)).and_then(|package| package.verify_filesystem());
    match result {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("runtime verification helper failed: {error}");
            Some(1)
        }
    }
}

fn write_response<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
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
    if request.schema_version != PROTOCOL_VERSION
        || request.request_id.trim().is_empty()
        || request.request_id.contains('\0')
    {
        return Err(failure(
            "invalid_request",
            "schemaVersion must be 2 and requestId must be non-empty",
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
        if !request.messages.is_empty()
            || !request.observations.is_empty()
            || !request.tools.is_empty()
            || request.staging_root.is_some()
        {
            return Err(failure(
                "invalid_request",
                "image_generate does not accept model messages, observations, or tools",
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
    if request.observations.is_empty() {
        if request.staging_root.is_some() {
            return Err(failure(
                "invalid_request",
                "stagingRoot is only valid when model_turn carries observations",
                false,
            ));
        }
    } else {
        let staging_root = request.staging_root.as_deref().ok_or_else(|| {
            failure(
                "invalid_request",
                "model_turn observations require the UpAgent-owned stagingRoot",
                false,
            )
        })?;
        validate_staging_root(staging_root)?;
    }
    for message in &request.messages {
        validate_message(message)?;
    }
    validate_observations(&request)?;
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
        if tool.input_schema.get("type") != Some(&Value::String("object".to_owned()))
            || tool.input_schema.get("nullable") == Some(&Value::Bool(true))
        {
            return Err(failure(
                "invalid_request",
                "tool inputSchema root must explicitly declare type=object",
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
        if schema_bytes.len() > CodexConfig::MIN_FRAME_BYTES {
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

#[derive(Debug)]
struct ValidatedObservation {
    message_index: usize,
    part_index: usize,
    detail: ProviderImageDetail,
    bytes: Vec<u8>,
}

fn validate_observations(request: &ProviderRequest) -> ProviderResult<()> {
    let image_parts = request
        .messages
        .iter()
        .flat_map(|message| message.content_parts.iter())
        .filter(|part| matches!(part, ProviderContentPart::ImageObservation { .. }))
        .count();
    if request.observations.len() > 16 {
        return Err(failure(
            "resource_limit",
            "model turn contains too many image observations",
            false,
        ));
    }
    if image_parts != request.observations.len() {
        return Err(failure(
            "invalid_request",
            "every image content part must have exactly one resolved observation",
            false,
        ));
    }
    let mut aggregate_bytes = 0_usize;
    let mut identities = std::collections::BTreeSet::new();
    for observation in &request.observations {
        let message = request
            .messages
            .get(observation.message_index)
            .ok_or_else(|| {
                failure(
                    "invalid_request",
                    "image observation message index is invalid",
                    false,
                )
            })?;
        let part = message
            .content_parts
            .get(observation.part_index)
            .ok_or_else(|| {
                failure(
                    "invalid_request",
                    "image observation part index is invalid",
                    false,
                )
            })?;
        let ProviderContentPart::ImageObservation { image } = part else {
            return Err(failure(
                "invalid_request",
                "resolved observation does not reference an image content part",
                false,
            ));
        };
        if !identities.insert((observation.message_index, observation.part_index)) {
            return Err(failure(
                "invalid_request",
                "duplicate image observation location",
                false,
            ));
        }
        validate_artifact_location(&image.artifact)?;
        if image.artifact.media_type != "image/png"
            || observation.media_type != image.artifact.media_type
            || observation.role != image.role
        {
            return Err(failure(
                "invalid_request",
                "image observation metadata does not match its content part",
                false,
            ));
        }
        let expected_length = usize::try_from(image.artifact.byte_length).map_err(|_| {
            failure(
                "resource_limit",
                "image observation byte length is not representable",
                false,
            )
        })?;
        if expected_length == 0
            || expected_length > MAX_IMAGE_BYTES
            || observation.base64.len() > MAX_IMAGE_BASE64_BYTES
        {
            return Err(failure(
                "resource_limit",
                "image observation exceeds the bounded byte limit",
                false,
            ));
        }
        let bytes = BASE64_STANDARD
            .decode(observation.base64.as_bytes())
            .map_err(|error| {
                failure(
                    "invalid_request",
                    &format!("image observation base64 is invalid: {error}"),
                    false,
                )
            })?;
        if bytes.is_empty() || bytes.len() != expected_length || bytes.len() > MAX_IMAGE_BYTES {
            return Err(failure(
                "invalid_request",
                "image observation decoded length does not match its artifact",
                false,
            ));
        }
        if observation.sha256 != image.artifact.sha256
            || !is_lower_hex_digest(&observation.sha256)
            || sha256_hex(&bytes) != observation.sha256
        {
            return Err(failure(
                "invalid_request",
                "image observation bytes do not match their SHA-256 identity",
                false,
            ));
        }
        let (width, height) = validate_png_dimensions(&bytes).map_err(|error| {
            failure(
                "invalid_request",
                &format!("image observation PNG validation failed: {error}"),
                false,
            )
        })?;
        if observation.width != width || observation.height != height {
            return Err(failure(
                "invalid_request",
                "image observation dimensions do not match the PNG payload",
                false,
            ));
        }
        aggregate_bytes = aggregate_bytes.checked_add(bytes.len()).ok_or_else(|| {
            failure(
                "resource_limit",
                "image observation aggregate byte count overflowed",
                false,
            )
        })?;
        if aggregate_bytes > 32 * 1024 * 1024 {
            return Err(failure(
                "resource_limit",
                "image observations exceed the 32 MiB aggregate byte bound",
                false,
            ));
        }
    }
    Ok(())
}

fn validated_observations(request: &ProviderRequest) -> ProviderResult<Vec<ValidatedObservation>> {
    validate_observations(request)?;
    let mut observations = Vec::with_capacity(request.observations.len());
    for observation in &request.observations {
        let message = request
            .messages
            .get(observation.message_index)
            .ok_or_else(|| {
                failure(
                    "invalid_request",
                    "image observation message index is invalid",
                    false,
                )
            })?;
        let part = message
            .content_parts
            .get(observation.part_index)
            .ok_or_else(|| {
                failure(
                    "invalid_request",
                    "image observation part index is invalid",
                    false,
                )
            })?;
        let ProviderContentPart::ImageObservation { image } = part else {
            return Err(failure(
                "invalid_request",
                "resolved observation does not reference an image content part",
                false,
            ));
        };
        let bytes = BASE64_STANDARD
            .decode(observation.base64.as_bytes())
            .map_err(|error| {
                failure(
                    "invalid_request",
                    &format!("image observation base64 is invalid: {error}"),
                    false,
                )
            })?;
        observations.push(ValidatedObservation {
            message_index: observation.message_index,
            part_index: observation.part_index,
            detail: image.detail,
            bytes,
        });
    }
    observations.sort_by_key(|observation| (observation.message_index, observation.part_index));
    Ok(observations)
}

fn validate_artifact_location(artifact: &ProviderArtifactRef) -> ProviderResult<()> {
    if artifact.id.is_empty()
        || artifact.id.len() > 512
        || artifact.id.bytes().any(|byte| byte.is_ascii_control())
        || artifact.relative_path.is_empty()
        || artifact.relative_path.len() > 4096
        || artifact.relative_path.contains('\0')
    {
        return Err(failure(
            "invalid_request",
            "image artifact location metadata is invalid",
            false,
        ));
    }
    let path = std::path::Path::new(&artifact.relative_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(failure(
            "invalid_request",
            "image artifact relativePath must stay within its artifact root",
            false,
        ));
    }
    Ok(())
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(request.timeout_ms))
        .ok_or_else(|| {
            failure(
                "invalid_request",
                "timeoutMs overflowed the monotonic clock",
                false,
            )
        })?;
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
        Duration::from_millis(request.timeout_ms),
        deadline,
    )
    .await?;
    let operation = match remaining_provider_timeout(deadline) {
        Err(error) => Err(error),
        Ok(operation_timeout) => match request.operation {
            ProviderOperation::ModelTurn => match timeout(
                operation_timeout,
                run_model_turn(&codex, &config, &request, deadline),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(failure(
                    "timeout",
                    "provider operation exceeded timeoutMs",
                    false,
                )),
            },
            // `Codex::generate_image` owns the dispatch state and receives this
            // absolute deadline through `CodexConfig`.  Do not wrap it in a
            // second timeout: dropping that future would erase whether the
            // image request was still pending or had reached the billed endpoint.
            ProviderOperation::ImageGenerate => run_image_generation(&codex, &request).await,
        },
    };
    let shutdown = match remaining_provider_timeout(deadline) {
        Ok(shutdown_timeout) => match timeout(shutdown_timeout, codex.shutdown()).await {
            Ok(result) => {
                result.map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))
            }
            Err(_) => Err(failure(
                "timeout",
                "provider shutdown exceeded timeoutMs",
                false,
            )),
        },
        Err(error) => match bounded_teardown(codex.shutdown()).await {
            Ok(Ok(())) => Err(error),
            Ok(Err(shutdown_error)) => Err(failure(
                "timeout",
                &format!(
                    "provider deadline expired; bounded teardown failed: {}",
                    classify_vergerail_error(&shutdown_error.to_string(), shutdown_error.kind())
                        .message
                ),
                false,
            )),
            Err(()) => Err(failure(
                "timeout",
                &format!(
                    "provider deadline expired; bounded teardown exceeded {} ms",
                    PROVIDER_TEARDOWN_BUDGET.as_millis()
                ),
                false,
            )),
        },
    };
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
    request_timeout: Duration,
    deadline: Instant,
) -> ProviderResult<Codex> {
    Codex::connect(
        CodexConfig::new(runtime)
            .with_image_generation(enable_images)
            .with_request_timeout(request_timeout)
            .with_shutdown_timeout(PROVIDER_TEARDOWN_BUDGET)
            .with_absolute_deadline(deadline)
            .with_max_frame_bytes(if enable_images {
                MAX_IMAGE_FRAME_BYTES
            } else {
                CodexConfig::MAX_FRAME_BYTES
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
    deadline: Instant,
) -> ProviderResult<Response> {
    let validated = validated_observations(request)?;
    let mut staging = ObservationStaging::create(&validated, request.staging_root.as_deref())?;
    let rendered = match render_model_prompt(request, &staging.entries) {
        Ok(rendered) => rendered,
        Err(error) => return finish_with_staging_cleanup(&mut staging, Err(error)),
    };
    let output_schema = match model_output_schema(request) {
        Ok(output_schema) => output_schema,
        Err(error) => return finish_with_staging_cleanup(&mut staging, Err(error)),
    };
    let mut options = match remaining_provider_timeout(deadline) {
        Ok(remaining) => SessionOptions::read_only(&config.workspace)
            .with_model(&config.model)
            .with_reasoning(map_reasoning(request.reasoning))
            .with_turn_timeout(remaining)
            .with_maximum_output_bytes(model_session_bytes(request.maximum_response_bytes))
            .with_output_schema(output_schema)
            .text_only(),
        Err(error) => return finish_with_staging_cleanup(&mut staging, Err(error)),
    };
    if let Some(base) = rendered.base_instructions {
        options = options.with_base_instructions(base);
    }
    if let Some(developer) = rendered.developer_instructions {
        options = options.with_developer_instructions(developer);
    }
    let session = match codex.session(options).await {
        Ok(session) => session,
        Err(error) => {
            let provider_error = classify_vergerail_error(&error.to_string(), error.kind());
            return finish_with_staging_cleanup(&mut staging, Err(provider_error));
        }
    };
    let mut inputs = vec![TurnInput::text(rendered.prompt)];
    inputs.extend(
        staging.entries.iter().map(|entry| {
            TurnInput::local_image(entry.path.clone(), Some(image_detail(entry.detail)))
        }),
    );
    let verification = async {
        let run = session
            .start(inputs)
            .await
            .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
        let result = wait_for_text_only_run(run).await?;
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
    let close = close.map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()));
    let cleanup = staging.cleanup();
    match (verification, close, cleanup) {
        (Ok(response), Ok(()), Ok(())) => Ok(response),
        (Err(error), Ok(()), Ok(())) => Err(error),
        (Ok(_), Err(error), Ok(())) => Err(error),
        (Err(operation_error), Err(close_error), Ok(())) => Err(failure(
            operation_error.code,
            &format!(
                "{}; model session close also failed: {}",
                operation_error.message, close_error.message
            ),
            false,
        )),
        (Ok(_), Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(operation_error), Ok(()), Err(cleanup_error)) => Err(failure(
            operation_error.code,
            &format!(
                "{}; observation staging cleanup also failed: {}",
                operation_error.message, cleanup_error.message
            ),
            false,
        )),
        (Ok(_), Err(close_error), Err(cleanup_error)) => Err(failure(
            cleanup_error.code,
            &format!(
                "model session close failed: {}; observation staging cleanup also failed: {}",
                close_error.message, cleanup_error.message
            ),
            false,
        )),
        (Err(operation_error), Err(close_error), Err(cleanup_error)) => Err(failure(
            operation_error.code,
            &format!(
                "{}; model session close also failed: {}; observation staging cleanup also failed: {}",
                operation_error.message, close_error.message, cleanup_error.message
            ),
            false,
        )),
    }
}

fn finish_with_staging_cleanup<T>(
    staging: &mut ObservationStaging,
    result: ProviderResult<T>,
) -> ProviderResult<T> {
    match (result, staging.cleanup()) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(failure(
            error.code,
            &format!(
                "{}; observation staging cleanup also failed: {}",
                error.message, cleanup_error.message
            ),
            false,
        )),
    }
}

async fn wait_for_text_only_run(mut run: Run) -> ProviderResult<RunResult> {
    let mut live_violation: Option<String> = None;
    while let Some(event) = run.next_event().await {
        match event.map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))? {
            Event::Started | Event::TextDelta(_) | Event::UsageUpdated(_) => {}
            Event::ApprovalRequested(request) => {
                request
                    .deny()
                    .await
                    .map_err(|error| classify_vergerail_error(&error.to_string(), error.kind()))?;
                live_violation.get_or_insert_with(|| "approval request".into());
            }
            Event::Command(_) | Event::CommandOutput(_) => {
                live_violation.get_or_insert_with(|| "command execution".into());
            }
            Event::FileChange(_) => {
                live_violation.get_or_insert_with(|| "file change".into());
            }
            Event::ImageGeneration(_) => {
                live_violation.get_or_insert_with(|| "image generation".into());
            }
            Event::Warning(_) => {
                live_violation.get_or_insert_with(|| "runtime warning".into());
            }
            Event::Unknown(event) if is_allowed_text_only_lifecycle(&event.method) => {}
            Event::Unknown(event) => {
                live_violation
                    .get_or_insert_with(|| format!("unsupported runtime event {}", event.method));
            }
            Event::Completed(result) => {
                if let Some(violation) = live_violation {
                    return Err(failure(
                        "provider_failed",
                        &format!("text-only model turn observed forbidden live {violation}"),
                        false,
                    ));
                }
                return Ok(result);
            }
            Event::Failed(error) => {
                return Err(classify_vergerail_error(&error.to_string(), error.kind()));
            }
            _ => {
                live_violation.get_or_insert_with(|| "unsupported runtime event".into());
            }
        }
    }
    Err(failure(
        "resolution_required",
        "text-only model turn disconnected before a terminal result",
        false,
    ))
}

fn is_allowed_text_only_lifecycle(method: &str) -> bool {
    matches!(
        method,
        "thread/status/changed"
            | "item/started"
            | "item/completed"
            | "turn/diff/updated"
            | "mcpServer/startupStatus/updated"
    )
}

fn remaining_provider_timeout(deadline: Instant) -> ProviderResult<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(failure("timeout", "provider deadline expired", false))
    } else {
        Ok(remaining)
    }
}

async fn bounded_teardown<F, T>(future: F) -> std::result::Result<T, ()>
where
    F: Future<Output = T>,
{
    bounded_teardown_with_budget(PROVIDER_TEARDOWN_BUDGET, future).await
}

async fn bounded_teardown_with_budget<F, T>(
    budget: Duration,
    future: F,
) -> std::result::Result<T, ()>
where
    F: Future<Output = T>,
{
    timeout(budget, future).await.map_err(|_| ())
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
    let provider_image = provider_image_from_response(
        generated.media_type(),
        generated.base64(),
        generated.byte_length(),
        generated.width(),
        generated.height(),
        generated.transparent_background(),
        request.maximum_response_bytes,
    )?;
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

fn provider_image_from_response(
    media_type: &str,
    result_base64: &str,
    byte_length: usize,
    width: u32,
    height: u32,
    transparent_background: Option<bool>,
    maximum_response_bytes: usize,
) -> ProviderResult<ProviderImage> {
    if result_base64.is_empty() || result_base64.len() > MAX_IMAGE_BASE64_BYTES {
        return Err(failure(
            "resource_limit",
            "encoded image exceeds the bounded output frame limit",
            false,
        ));
    }
    if byte_length == 0 || byte_length > MAX_IMAGE_BYTES || byte_length > maximum_response_bytes {
        return Err(failure(
            "resource_limit",
            "decoded image exceeds the bounded output limit",
            false,
        ));
    }
    Ok(ProviderImage {
        media_type: media_type.to_owned(),
        base64: result_base64.to_owned(),
        byte_length,
        width,
        height,
        transparent_background,
    })
}

#[derive(Debug)]
struct StagedObservation {
    message_index: usize,
    part_index: usize,
    detail: ProviderImageDetail,
    path: PathBuf,
}

#[derive(Debug)]
struct ObservationStaging {
    root: Option<PathBuf>,
    entries: Vec<StagedObservation>,
    cleaned: bool,
}

impl ObservationStaging {
    fn create(
        observations: &[ValidatedObservation],
        requested_root: Option<&std::path::Path>,
    ) -> ProviderResult<Self> {
        if observations.is_empty() {
            if requested_root.is_some() {
                return Err(failure(
                    "invalid_request",
                    "stagingRoot is only valid when observations are present",
                    false,
                ));
            }
            return Ok(Self {
                root: None,
                entries: Vec::new(),
                cleaned: true,
            });
        }
        let root = requested_root.ok_or_else(|| {
            failure(
                "invalid_request",
                "observations require the UpAgent-owned stagingRoot",
                false,
            )
        })?;
        validate_staging_root(root)?;
        let mut staging = Self {
            root: Some(root.to_owned()),
            entries: Vec::with_capacity(observations.len()),
            cleaned: false,
        };
        let result = (|| -> ProviderResult<()> {
            for (index, observation) in observations.iter().enumerate() {
                let path = root.join(format!("observation-{index}.png"));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|error| {
                        failure(
                            "provider_failed",
                            &format!("could not create private image staging file: {error}"),
                            false,
                        )
                    })?;
                set_private_file_permissions(&file).map_err(|error| {
                    failure(
                        "provider_failed",
                        &format!("could not secure private image staging file: {error}"),
                        false,
                    )
                })?;
                file.write_all(&observation.bytes).map_err(|error| {
                    failure(
                        "provider_failed",
                        &format!("could not write private image staging file: {error}"),
                        false,
                    )
                })?;
                file.sync_all().map_err(|error| {
                    failure(
                        "provider_failed",
                        &format!("could not flush private image staging file: {error}"),
                        false,
                    )
                })?;
                staging.entries.push(StagedObservation {
                    message_index: observation.message_index,
                    part_index: observation.part_index,
                    detail: observation.detail,
                    path,
                });
            }
            Ok(())
        })();
        if let Err(error) = result {
            return finish_with_staging_cleanup(&mut staging, Err(error));
        }
        Ok(staging)
    }

    fn cleanup(&mut self) -> ProviderResult<()> {
        if self.cleaned {
            return Ok(());
        }
        let Some(root) = self.root.as_deref() else {
            self.cleaned = true;
            return Ok(());
        };
        match fs::remove_dir_all(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(failure(
                    "provider_failed",
                    &format!("could not remove private image staging directory: {error}"),
                    false,
                ));
            }
        }
        self.cleaned = true;
        Ok(())
    }
}

fn set_private_file_permissions(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn image_detail(detail: ProviderImageDetail) -> ImageDetail {
    match detail {
        ProviderImageDetail::Low => ImageDetail::Low,
        ProviderImageDetail::Auto => ImageDetail::Auto,
        ProviderImageDetail::High => ImageDetail::High,
    }
}

struct RenderedModelPrompt {
    prompt: String,
    base_instructions: Option<String>,
    developer_instructions: Option<String>,
}

fn render_prompt_message(
    message_index: usize,
    message: &ProviderMessage,
    observations: &[StagedObservation],
) -> ProviderResult<Value> {
    let mut value = serde_json::to_value(message).map_err(|error| {
        failure(
            "invalid_request",
            &format!("could not encode provider-neutral message: {error}"),
            false,
        )
    })?;
    let mut parts = Vec::with_capacity(message.content_parts.len());
    for (part_index, part) in message.content_parts.iter().enumerate() {
        match part {
            ProviderContentPart::Text { text } => {
                parts.push(serde_json::json!({"type": "text", "text": text}));
            }
            ProviderContentPart::ImageObservation { image } => {
                let observation_index = observations
                    .iter()
                    .position(|observation| {
                        observation.message_index == message_index
                            && observation.part_index == part_index
                    })
                    .ok_or_else(|| {
                        failure(
                            "invalid_request",
                            "image prompt mapping is missing a staged observation",
                            false,
                        )
                    })?;
                parts.push(serde_json::json!({
                    "type": "imageObservation",
                    "observationIndex": observation_index,
                    "role": image.role,
                    "detail": image.detail,
                    "caption": image.caption,
                }));
            }
        }
    }
    if !parts.is_empty() {
        value["contentParts"] = Value::Array(parts);
    }
    Ok(value)
}

fn render_model_prompt(
    request: &ProviderRequest,
    observations: &[StagedObservation],
) -> ProviderResult<RenderedModelPrompt> {
    let mut base = Vec::new();
    let mut developer = Vec::new();
    let mut transcript = Vec::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        match message.role {
            ProviderMessageRole::System => base.push(message.content.clone()),
            ProviderMessageRole::Developer => developer.push(message.content.clone()),
            ProviderMessageRole::User
            | ProviderMessageRole::Assistant
            | ProviderMessageRole::Tool => {
                transcript.push(render_prompt_message(message_index, message, observations)?);
            }
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
            normalize_native_const_type(object)?;
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
                            make_native_property_nullable(property)?;
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

fn native_json_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::Null => Some("null"),
        Value::Bool(_) => Some("boolean"),
        Value::Number(number) if number_is_integral(number) => Some("integer"),
        Value::Number(_) => Some("number"),
        Value::String(_) => Some("string"),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn number_is_integral(number: &serde_json::Number) -> bool {
    number.is_i64()
        || number.is_u64()
        || number
            .as_f64()
            .is_some_and(|value| value.is_finite() && value.fract() == 0.0)
}

fn normalize_native_const_type(object: &mut serde_json::Map<String, Value>) -> ProviderResult<()> {
    let Some(constant) = object.get("const").cloned() else {
        return Ok(());
    };
    let Some(kind) = native_json_type(&constant) else {
        return Err(failure(
            "invalid_request",
            "native strict schemas do not support array or object const values",
            false,
        ));
    };
    validate_const_enum_membership(object, &constant)?;
    if let Some(type_value) = object.get("type")
        && !native_type_includes(type_value, kind)
    {
        return Err(failure(
            "invalid_request",
            "native strict schema const value does not match its declared type",
            false,
        ));
    }
    object
        .entry("type".to_owned())
        .or_insert_with(|| Value::String(kind.to_owned()));
    Ok(())
}

fn validate_const_enum_membership(
    object: &serde_json::Map<String, Value>,
    constant: &Value,
) -> ProviderResult<()> {
    let Some(enum_value) = object.get("enum") else {
        return Ok(());
    };
    let values = enum_value.as_array().ok_or_else(|| {
        failure(
            "invalid_request",
            "native strict schema enum must be an array when const is present",
            false,
        )
    })?;
    if values
        .iter()
        .any(|value| primitive_schema_values_equal(value, constant))
    {
        Ok(())
    } else {
        Err(failure(
            "invalid_request",
            "native strict schema const value is excluded by enum",
            false,
        ))
    }
}

fn primitive_schema_values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::String(_), Value::String(_)) => left == right,
        (Value::Number(left), Value::Number(right)) => numeric_schema_values_equal(left, right),
        _ => false,
    }
}

fn numeric_schema_values_equal(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    canonical_decimal(left) == canonical_decimal(right)
}

#[derive(Debug, Eq, PartialEq)]
struct CanonicalDecimal {
    negative: bool,
    digits: String,
    scale: i32,
}

fn canonical_decimal(number: &serde_json::Number) -> Option<CanonicalDecimal> {
    let text = number.to_string();
    let exponent_start = text.find(['e', 'E']);
    let (mantissa, exponent) = match exponent_start {
        Some(index) => (&text[..index], text[index + 1..].parse::<i32>().ok()?),
        None => (text.as_str(), 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa
        .strip_prefix('-')
        .or_else(|| mantissa.strip_prefix('+'))
        .unwrap_or(mantissa);
    let decimal_start = mantissa.find('.');
    let fractional_digits = match decimal_start {
        Some(index) => i32::try_from(mantissa.len() - index - 1).ok()?,
        None => 0,
    };
    let digits = mantissa.replace('.', "");
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let first_nonzero = digits.bytes().position(|byte| byte != b'0');
    let Some(first_nonzero) = first_nonzero else {
        return Some(CanonicalDecimal {
            negative: false,
            digits: "0".to_owned(),
            scale: 0,
        });
    };
    let mut digits = digits[first_nonzero..].to_owned();
    let mut scale = exponent.checked_sub(fractional_digits)?;
    while digits.ends_with('0') {
        digits.pop();
        scale = scale.checked_add(1)?;
    }
    Some(CanonicalDecimal {
        negative,
        digits,
        scale,
    })
}

fn native_type_includes(type_value: &Value, expected: &str) -> bool {
    match type_value {
        Value::String(kind) => native_type_pair_is_compatible(kind, expected),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| native_type_pair_is_compatible(kind, expected)),
        _ => false,
    }
}

fn native_type_pair_is_compatible(declared: &str, actual: &str) -> bool {
    declared == actual || (declared == "number" && actual == "integer")
}

fn make_native_property_nullable(schema: &mut Value) -> ProviderResult<()> {
    if let Value::Object(object) = schema {
        if let Some(constant) = object.remove("const") {
            let Some(kind) = native_json_type(&constant) else {
                return Err(failure(
                    "invalid_request",
                    "native strict schemas do not support array or object const values",
                    false,
                ));
            };
            validate_const_enum_membership(object, &constant)?;
            if let Some(type_value) = object.get("type")
                && !native_type_includes(type_value, kind)
            {
                return Err(failure(
                    "invalid_request",
                    "native strict schema const value does not match its declared type",
                    false,
                ));
            }
            object
                .entry("type".to_owned())
                .or_insert_with(|| Value::String(kind.to_owned()));
            object.remove("enum");
            let mut enum_values = vec![constant];
            if kind != "null" {
                enum_values.push(Value::Null);
            }
            object.insert("enum".to_owned(), Value::Array(enum_values));
        }
        if let Some(type_value) = object.get("type")
            && (type_value.as_str() == Some("null")
                || type_value
                    .as_array()
                    .is_some_and(|types| types.iter().any(|value| value.as_str() == Some("null"))))
        {
            if let Some(enum_values) = object.get_mut("enum").and_then(Value::as_array_mut)
                && !enum_values.iter().any(Value::is_null)
            {
                enum_values.push(Value::Null);
            }
            return Ok(());
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
            return Ok(());
        }
        if let Some(type_value) = object.get_mut("type") {
            match type_value {
                Value::String(kind) => {
                    let kind = std::mem::take(kind);
                    *type_value =
                        Value::Array(vec![Value::String(kind), Value::String("null".to_owned())]);
                }
                Value::Array(types) => {
                    if !types.iter().any(|value| value.as_str() == Some("null")) {
                        types.push(Value::String("null".to_owned()));
                    }
                }
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => {}
            }
            if let Some(enum_values) = object.get_mut("enum").and_then(Value::as_array_mut)
                && !enum_values.iter().any(Value::is_null)
            {
                enum_values.push(Value::Null);
            }
            return Ok(());
        }
    }
    let original = schema.clone();
    *schema = serde_json::json!({
        "anyOf": [original, {"type": "null"}]
    });
    Ok(())
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

fn classify_vergerail_error(message: &str, kind: vergerail::ErrorKind) -> ProviderFailure {
    let code = match kind {
        vergerail::ErrorKind::Timeout => "timeout",
        vergerail::ErrorKind::OutcomeUnknown
        | vergerail::ErrorKind::Disconnected
        | vergerail::ErrorKind::ConsumerLagged => "resolution_required",
        vergerail::ErrorKind::Authentication => "authentication_required",
        vergerail::ErrorKind::Cancelled => "cancelled",
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
    use std::io;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed stdout"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("closed stdout"))
        }
    }

    fn request(operation: ProviderOperation) -> ProviderRequest {
        ProviderRequest {
            schema_version: 2,
            request_id: "request-1".to_owned(),
            operation,
            messages: vec![ProviderMessage {
                role: ProviderMessageRole::User,
                content: "hello".to_owned(),
                content_parts: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                is_error: false,
            }],
            observations: Vec::new(),
            staging_root: None,
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
        assert_eq!(valid.schema_version, PROTOCOL_VERSION);

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
    fn stdout_write_failure_is_reported_to_the_process_boundary() {
        assert!(write_response(&mut FailingWriter, b"{}\n").is_err());
    }

    #[tokio::test]
    async fn expired_deadline_teardown_is_bounded_by_its_separate_budget() {
        let started = Instant::now();
        let result = bounded_teardown_with_budget(
            Duration::from_millis(10),
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;
        assert!(result.is_err(), "teardown must stop at its fixed budget");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(PROVIDER_TEARDOWN_BUDGET, Duration::from_secs(2));
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
    fn native_output_schema_infers_primitive_types_for_const_properties() {
        let mut value = request(ProviderOperation::ModelTurn);
        value.tools.push(ProviderTool {
            name: "edit_asset".to_owned(),
            description: "Edit one image".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "mediaType"],
                "properties": {
                    "operation": {"const": "remove_background"},
                    "mediaType": {"const": "image/png"}
                }
            }),
            strict: true,
        });
        let schema = model_output_schema(&value).expect("native schema");
        for property in ["operation", "mediaType"] {
            assert_eq!(
                schema.pointer(&format!(
                    "/properties/toolCalls/properties/edit_asset/items/properties/arguments/properties/{property}/type"
                )),
                Some(&json!("string"))
            );
        }
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/edit_asset/items/properties/arguments/properties/operation/const"
            ),
            Some(&json!("remove_background"))
        );
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/edit_asset/items/properties/arguments/properties/operation/enum"
            ),
            None
        );
    }

    #[test]
    fn native_output_schema_normalizes_optional_primitive_consts_to_nullable_enums() {
        for (constant, expected_type) in [
            (json!("remove_background"), json!("string")),
            (json!(7), json!("integer")),
            (json!(true), json!("boolean")),
        ] {
            let mut value = request(ProviderOperation::ModelTurn);
            value.tools.push(ProviderTool {
                name: "optional_const".to_owned(),
                description: "Optional primitive const".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"value": {"const": constant}}
                }),
                strict: true,
            });
            let schema = model_output_schema(&value).expect("native schema");
            let base = "/properties/toolCalls/properties/optional_const/items/properties/arguments/properties/value";
            assert_eq!(
                schema.pointer(&format!("{base}/type")),
                Some(&json!([expected_type, "null"]))
            );
            assert_eq!(
                schema.pointer(&format!("{base}/enum")),
                Some(&json!([constant, null]))
            );
            assert_eq!(schema.pointer(&format!("{base}/const")), None);
        }
    }

    #[test]
    fn native_output_schema_respects_json_number_integer_compatibility() {
        let mut required_number = request(ProviderOperation::ModelTurn);
        required_number.tools.push(ProviderTool {
            name: "number_const".to_owned(),
            description: "Number const".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "number", "const": 7}}
            }),
            strict: true,
        });
        let schema = model_output_schema(&required_number).expect("number const schema");
        let base = "/properties/toolCalls/properties/number_const/items/properties/arguments/properties/value";
        assert_eq!(
            schema.pointer(&format!("{base}/type")),
            Some(&json!("number"))
        );
        assert_eq!(schema.pointer(&format!("{base}/const")), Some(&json!(7)));

        let mut optional_number = required_number.clone();
        optional_number.tools[0].input_schema["required"] = json!([]);
        let schema = model_output_schema(&optional_number).expect("optional number const schema");
        assert_eq!(
            schema.pointer(&format!("{base}/type")),
            Some(&json!(["number", "null"]))
        );
        assert_eq!(
            schema.pointer(&format!("{base}/enum")),
            Some(&json!([7, null]))
        );
        assert_eq!(schema.pointer(&format!("{base}/const")), None);

        for constant in [json!(7.0), json!(7e0)] {
            let mut integral = request(ProviderOperation::ModelTurn);
            integral.tools.push(ProviderTool {
                name: "integral_const".to_owned(),
                description: "Integral number const".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["value"],
                    "properties": {"value": {"type": "integer", "const": constant}}
                }),
                strict: true,
            });
            let schema = model_output_schema(&integral).expect("integral number const schema");
            assert_eq!(
                schema.pointer(
                    "/properties/toolCalls/properties/integral_const/items/properties/arguments/properties/value/type"
                ),
                Some(&json!("integer"))
            );
        }

        let mut fractional = request(ProviderOperation::ModelTurn);
        fractional.tools.push(ProviderTool {
            name: "fractional_const".to_owned(),
            description: "Fractional number const".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "integer", "const": 7.5}}
            }),
            strict: true,
        });
        assert_eq!(
            model_output_schema(&fractional)
                .expect_err("fractional const cannot satisfy integer")
                .code,
            "invalid_request"
        );

        let mut fractional_number = request(ProviderOperation::ModelTurn);
        fractional_number.tools.push(ProviderTool {
            name: "fractional_number".to_owned(),
            description: "Fractional number const".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "number", "const": 7.5}}
            }),
            strict: true,
        });
        let schema = model_output_schema(&fractional_number).expect("fractional number schema");
        assert_eq!(
            schema.pointer(
                "/properties/toolCalls/properties/fractional_number/items/properties/arguments/properties/value/type"
            ),
            Some(&json!("number"))
        );

        let mut number_union = request(ProviderOperation::ModelTurn);
        number_union.tools.push(ProviderTool {
            name: "number_union".to_owned(),
            description: "Number/integer const".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": ["number", "null"], "const": 7}}
            }),
            strict: true,
        });
        model_output_schema(&number_union).expect("number union accepts integer const");

        let mut compatible_enum = request(ProviderOperation::ModelTurn);
        compatible_enum.tools.push(ProviderTool {
            name: "compatible_enum".to_owned(),
            description: "Numerically compatible const enum".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "number", "const": 7, "enum": [7.0]}}
            }),
            strict: true,
        });
        let schema = model_output_schema(&compatible_enum).expect("7 and 7.0 are equal");
        let base = "/properties/toolCalls/properties/compatible_enum/items/properties/arguments/properties/value";
        assert_eq!(
            schema.pointer(&format!("{base}/type")),
            Some(&json!("number"))
        );
        assert_eq!(schema.pointer(&format!("{base}/const")), Some(&json!(7)));
        assert_eq!(schema.pointer(&format!("{base}/enum")), Some(&json!([7.0])));

        let mut optional_compatible_enum = compatible_enum.clone();
        optional_compatible_enum.tools[0].input_schema["required"] = json!([]);
        optional_compatible_enum.tools[0].input_schema["properties"]["value"]["enum"] =
            json!([7e0, 8]);
        let schema = model_output_schema(&optional_compatible_enum)
            .expect("optional 7 and exponent 7 are equal");
        assert_eq!(
            schema.pointer(&format!("{base}/type")),
            Some(&json!(["number", "null"]))
        );
        assert_eq!(
            schema.pointer(&format!("{base}/enum")),
            Some(&json!([7, null]))
        );
        assert_eq!(schema.pointer(&format!("{base}/const")), None);

        let mut incompatible_enum = request(ProviderOperation::ModelTurn);
        incompatible_enum.tools.push(ProviderTool {
            name: "incompatible_enum".to_owned(),
            description: "Numerically incompatible const enum".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "number", "const": 7, "enum": [7.5]}}
            }),
            strict: true,
        });
        assert_eq!(
            model_output_schema(&incompatible_enum)
                .expect_err("7 and 7.5 are not equal")
                .code,
            "invalid_request"
        );

        let mut large_integer = request(ProviderOperation::ModelTurn);
        large_integer.tools.push(ProviderTool {
            name: "large_integer".to_owned(),
            description: "Lossless integer equality".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {
                    "value": {
                        "type": "number",
                        "const": 9_007_199_254_740_993u64,
                        "enum": [9_007_199_254_740_992.0]
                    }
                }
            }),
            strict: true,
        });
        assert_eq!(
            model_output_schema(&large_integer)
                .expect_err("lossy float must not equal a distinct large integer")
                .code,
            "invalid_request"
        );

        let mut negative_zero = request(ProviderOperation::ModelTurn);
        negative_zero.tools.push(ProviderTool {
            name: "negative_zero".to_owned(),
            description: "Signed zero equality".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "number", "const": 0, "enum": [-0.0]}}
            }),
            strict: true,
        });
        model_output_schema(&negative_zero).expect("-0 and 0 are equal");

        let mut integer_union = request(ProviderOperation::ModelTurn);
        integer_union.tools.push(ProviderTool {
            name: "integer_union".to_owned(),
            description: "Integer/number const".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": ["integer", "null"], "const": 7.5}}
            }),
            strict: true,
        });
        assert_eq!(
            model_output_schema(&integer_union)
                .expect_err("integer union rejects fractional const")
                .code,
            "invalid_request"
        );
    }

    #[test]
    fn native_output_schema_rejects_unsupported_const_values_and_is_idempotent() {
        let mut unsupported = request(ProviderOperation::ModelTurn);
        unsupported.tools.push(ProviderTool {
            name: "unsupported_const".to_owned(),
            description: "Unsupported const".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"value": {"const": {"nested": true}}}
            }),
            strict: true,
        });
        assert_eq!(
            model_output_schema(&unsupported)
                .expect_err("array/object const must be rejected")
                .code,
            "invalid_request"
        );

        let mut schema = json!({
            "type": "object",
            "properties": {"value": {"const": "x", "enum": ["x", "other"]}}
        });
        close_native_object_schemas(&mut schema).expect("first normalization");
        let normalized = schema.clone();
        close_native_object_schemas(&mut schema).expect("second normalization");
        assert_eq!(schema, normalized);
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
    fn tool_schema_root_must_be_a_non_nullable_object() {
        for root in [json!({}), json!("object"), json!([]), json!(null)] {
            let mut value = request(ProviderOperation::ModelTurn);
            value.tools.push(ProviderTool {
                name: "invalid_root".to_owned(),
                description: "Invalid root".to_owned(),
                input_schema: root,
                strict: true,
            });
            assert_eq!(
                validate_request(value)
                    .expect_err("tool root must be explicitly object")
                    .code,
                "invalid_request"
            );
        }
        let mut nullable = request(ProviderOperation::ModelTurn);
        nullable.tools.push(ProviderTool {
            name: "nullable_root".to_owned(),
            description: "Nullable root".to_owned(),
            input_schema: json!({"type": ["object", "null"]}),
            strict: true,
        });
        assert_eq!(
            validate_request(nullable)
                .expect_err("nullable root must be rejected")
                .code,
            "invalid_request"
        );
        let mut nullable_flag = request(ProviderOperation::ModelTurn);
        nullable_flag.tools.push(ProviderTool {
            name: "nullable_flag".to_owned(),
            description: "Nullable root flag".to_owned(),
            input_schema: json!({"type": "object", "nullable": true}),
            strict: true,
        });
        assert_eq!(
            validate_request(nullable_flag)
                .expect_err("nullable root flag must be rejected")
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
    fn image_payload_forwards_validated_metadata_and_enforces_caps() {
        let image = provider_image_from_response(
            "image/png",
            "already-validated-base64",
            123,
            2,
            3,
            Some(true),
            123,
        )
        .expect("validated image metadata");
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.base64, "already-validated-base64");
        assert_eq!(image.byte_length, 123);
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.transparent_background, Some(true));

        let oversized_encoded = "x".repeat(MAX_IMAGE_BASE64_BYTES + 1);
        assert_eq!(
            provider_image_from_response(
                "image/png",
                &oversized_encoded,
                1,
                1,
                1,
                None,
                MAX_IMAGE_BYTES,
            )
            .expect_err("encoded image cap")
            .code,
            "resource_limit"
        );
        assert_eq!(
            provider_image_from_response(
                "image/png",
                "x",
                MAX_IMAGE_BYTES + 1,
                1,
                1,
                None,
                MAX_IMAGE_BYTES,
            )
            .expect_err("decoded image cap")
            .code,
            "resource_limit"
        );
        assert_eq!(
            provider_image_from_response("image/png", "x", 2, 1, 1, None, 1)
                .expect_err("caller image cap")
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
        assert_ne!(MAX_IMAGE_FRAME_BYTES, CodexConfig::MAX_FRAME_BYTES);
    }

    #[test]
    fn protocol_schema_is_versioned() {
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn text_only_lifecycle_allows_only_known_non_effect_notifications() {
        for method in [
            "thread/status/changed",
            "item/started",
            "item/completed",
            "turn/diff/updated",
            "mcpServer/startupStatus/updated",
        ] {
            assert!(is_allowed_text_only_lifecycle(method), "{method}");
        }
        assert!(!is_allowed_text_only_lifecycle(
            "item/commandExecution/started"
        ));
        assert!(!is_allowed_text_only_lifecycle("unknown/additive"));
    }
}
