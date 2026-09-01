//! Black-box stdin/stdout contract tests for the UpAgent provider process.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn provider() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_vergerail-upagent-provider")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_vergerail_upagent_provider"))
        .map(PathBuf::from)
        .expect("Cargo must expose the vergerail-upagent-provider test binary")
}

fn model_request(reasoning: &str, maximum_response_bytes: usize) -> Value {
    json!({
        "schemaVersion": 2,
        "requestId": "protocol-model-1",
        "operation": "model_turn",
        "messages": [{
            "role": "user",
            "content": "Return one compact response.",
            "toolCalls": [],
            "toolCallId": null,
            "toolName": null,
            "isError": false
        }],
        "tools": [],
        "reasoning": reasoning,
        "timeoutMs": 1000,
        "maximumResponseBytes": maximum_response_bytes,
        "prompt": null
    })
}

fn image_request(reasoning: &str) -> Value {
    json!({
        "schemaVersion": 2,
        "requestId": "protocol-image-1",
        "operation": "image_generate",
        "messages": [],
        "tools": [],
        "reasoning": reasoning,
        "timeoutMs": 1000,
        "maximumResponseBytes": 8 * 1024 * 1024,
        "prompt": "Generate exactly one transparent PNG."
    })
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for byte in chunk[4..].iter().copied() {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    chunk
}

fn png_bytes(size: usize) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    std::io::Write::write_all(&mut encoder, &[0, 255, 0, 0, 255]).expect("PNG row");
    let compressed = encoder.finish().expect("PNG zlib stream");
    let ihdr = [
        0, 0, 0, 1, // width
        0, 0, 0, 1, // height
        8, 6, 0, 0, 0, // RGBA8, no interlace
    ];
    let idat = png_chunk(b"IDAT", &compressed);
    let iend = png_chunk(b"IEND", &[]);
    let fixed_len = 8 + png_chunk(b"IHDR", &ihdr).len() + idat.len() + iend.len();
    assert!(
        size >= fixed_len + 16,
        "padded PNG fixture must fit ancillary chunk"
    );
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    let padding_data_len = size - fixed_len - 12;
    let mut padding = Vec::with_capacity(padding_data_len);
    padding.extend_from_slice(b"pad\0");
    padding.resize(padding_data_len, b'x');
    bytes.extend_from_slice(&png_chunk(b"tEXt", &padding));
    bytes.extend_from_slice(&idat);
    bytes.extend_from_slice(&iend);
    assert_eq!(bytes.len(), size);
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn observation_request(sizes: &[usize], staging_root: &std::path::Path) -> Value {
    let mut parts = Vec::with_capacity(sizes.len());
    let mut observations = Vec::with_capacity(sizes.len());
    for (part_index, size) in sizes.iter().copied().enumerate() {
        let bytes = png_bytes(size);
        let digest = sha256_hex(&bytes);
        let base64 = BASE64_STANDARD.encode(&bytes);
        let artifact_id = format!("art-{digest}-{}", "b".repeat(64));
        parts.push(json!({
            "type": "image_observation",
            "image": {
                "artifact": {
                    "id": artifact_id,
                    "sha256": digest,
                    "mediaType": "image/png",
                    "byteLength": size,
                    "relativePath": format!("aa/{artifact_id}.png")
                },
                "role": "full",
                "detail": "auto"
            }
        }));
        observations.push(json!({
            "messageIndex": 0,
            "partIndex": part_index,
            "mediaType": "image/png",
            "sha256": digest,
            "width": 1,
            "height": 1,
            "role": "full",
            "base64": base64
        }));
    }
    let mut request = model_request("medium", 8 * 1024 * 1024);
    request["messages"][0]["contentParts"] = Value::Array(parts);
    request["observations"] = Value::Array(observations);
    request["stagingRoot"] = json!(staging_root);
    request
}

fn staging_root() -> TempDir {
    let root = tempfile::Builder::new()
        .prefix("vergerail-upagent-request-")
        .tempdir()
        .expect("request staging root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("private request staging root");
    }
    root
}

fn replace_observation_payload(request: &mut Value, bytes: &[u8]) {
    let digest = sha256_hex(bytes);
    request["observations"][0]["base64"] = json!(BASE64_STANDARD.encode(bytes));
    request["observations"][0]["sha256"] = json!(&digest);
    request["observations"][0]["width"] = json!(1);
    request["observations"][0]["height"] = json!(1);
    request["messages"][0]["contentParts"][0]["image"]["artifact"]["sha256"] = json!(&digest);
    request["messages"][0]["contentParts"][0]["image"]["artifact"]["byteLength"] =
        json!(bytes.len());
}

fn truncate_idat_data(bytes: Vec<u8>) -> Vec<u8> {
    let idat_type = bytes
        .windows(4)
        .position(|window| window == b"IDAT")
        .expect("IDAT chunk");
    let chunk_start = idat_type - 4;
    let length = u32::from_be_bytes(
        bytes[chunk_start..chunk_start + 4]
            .try_into()
            .expect("IDAT length"),
    ) as usize;
    let data_start = idat_type + 4;
    let chunk_end = data_start + length + 4;
    let replacement = png_chunk(b"IDAT", &bytes[data_start..data_start + 1]);
    let mut truncated = bytes[..chunk_start].to_vec();
    truncated.extend_from_slice(&replacement);
    truncated.extend_from_slice(&bytes[chunk_end..]);
    truncated
}

fn execute(value: &Value) -> Output {
    let mut child = Command::new(provider())
        .env_remove("VERGERAIL_CODEX_PACKAGE")
        .env_remove("VERGERAIL_MODEL")
        .env_remove("VERGERAIL_WORKSPACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("provider process must start");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(&serde_json::to_vec(value).expect("request JSON"))
        .expect("request input");
    child.wait_with_output().expect("provider output")
}

fn response(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "typed failures are successful process transport: {output:?}"
    );
    assert!(
        output.stderr.is_empty(),
        "protocol responses must not write stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "stdout must contain exactly one JSON value");
    serde_json::from_slice(lines[0]).expect("strict JSON response")
}

#[test]
fn missing_environment_fails_before_runtime_access() {
    let result = response(&execute(&model_request("medium", 1024)));
    assert_eq!(result["schemaVersion"], 2);
    assert_eq!(result["requestId"], "protocol-model-1");
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"]["code"], "invalid_configuration");
    assert_eq!(result["error"]["retryable"], false);
}

#[test]
fn unknown_fields_fail_before_environment_or_runtime_access() {
    let mut request = model_request("medium", 1024);
    request["shell"] = json!("/bin/zsh");
    let result = response(&execute(&request));
    assert_eq!(result["error"]["code"], "invalid_request");
    assert_eq!(result["error"]["retryable"], false);
}

#[test]
fn invalid_request_id_is_not_reflected_into_the_response() {
    let mut request = model_request("medium", 1024);
    request["requestId"] = json!("x".repeat(256));
    let output = execute(&request);
    let result = response(&output);
    assert_eq!(result["requestId"], "");
    assert_eq!(result["error"]["code"], "invalid_request");
    assert!(output.stdout.len() < 4096);
}

#[test]
fn all_six_reasoning_values_are_wire_values_for_both_operations() {
    for reasoning in ["off", "low", "medium", "high", "xhigh", "max"] {
        for request in [
            model_request(reasoning, 8 * 1024 * 1024),
            image_request(reasoning),
        ] {
            let result = response(&execute(&request));
            assert_eq!(
                result["error"]["code"], "invalid_configuration",
                "reasoning must parse and pass request validation: {reasoning}"
            );
        }
    }
}

#[test]
fn omitted_reasoning_is_rejected_before_runtime_access() {
    let mut request = model_request("medium", 1024);
    request
        .as_object_mut()
        .expect("request object")
        .remove("reasoning");
    let result = response(&execute(&request));
    assert_eq!(result["requestId"], "protocol-model-1");
    assert_eq!(result["error"]["code"], "invalid_request");
}

#[test]
fn caller_response_caps_are_exact() {
    let accepted = response(&execute(&model_request("high", 8 * 1024 * 1024)));
    assert_eq!(accepted["error"]["code"], "invalid_configuration");

    let rejected = response(&execute(&model_request("high", 8 * 1024 * 1024 + 1)));
    assert_eq!(rejected["error"]["code"], "invalid_request");

    let mut image = image_request("high");
    image["maximumResponseBytes"] = json!(8 * 1024 * 1024 + 1);
    let output = execute(&image);
    assert!(output.stdout.len() <= 16 * 1024);
    let rejected = response(&output);
    assert_eq!(rejected["error"]["code"], "invalid_request");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .expect("failure message")
            .len()
            <= 4 * 1024
    );
}

#[test]
fn malformed_tool_contract_is_rejected_without_runtime_access() {
    let mut request = model_request("medium", 1024);
    request["tools"] = json!([{
        "name": "inspect_asset",
        "description": "Inspect an image",
        "inputSchema": {"type": "object"},
        "strict": false
    }]);
    let result = response(&execute(&request));
    assert_eq!(result["error"]["code"], "invalid_request");

    request["tools"][0]["strict"] = json!(true);
    request["tools"][0]["inputSchema"] = json!([]);
    let result = response(&execute(&request));
    assert_eq!(result["error"]["code"], "invalid_request");
}

#[test]
fn actual_binary_admits_base64_observations_at_each_supported_size() {
    for sizes in [
        vec![512 * 1024],
        vec![1024 * 1024],
        vec![8 * 1024 * 1024],
        vec![8 * 1024 * 1024; 4],
    ] {
        let root = staging_root();
        let request = observation_request(&sizes, root.path());
        assert!(serde_json::to_vec(&request).expect("request JSON").len() <= 64 * 1024 * 1024);
        let result = response(&execute(&request));
        assert_eq!(result["schemaVersion"], 2);
        assert_eq!(
            result["error"]["code"], "invalid_configuration",
            "sizes={sizes:?} result={result}"
        );
    }
}

#[test]
fn actual_binary_rejects_legacy_schema_and_invalid_observation_before_runtime_access() {
    let root = staging_root();
    let mut legacy = model_request("medium", 1024);
    legacy["stagingRoot"] = json!(root.path());
    legacy["schemaVersion"] = json!(1);
    let result = response(&execute(&legacy));
    assert_eq!(result["schemaVersion"], 2);
    assert_eq!(result["error"]["code"], "invalid_request");

    let mut invalid = observation_request(&[512 * 1024], root.path());
    invalid["observations"][0]["base64"] = json!("not-base64");
    let result = response(&execute(&invalid));
    assert_eq!(result["error"]["code"], "invalid_request");
}

#[test]
fn actual_binary_rejects_malformed_png_structure_before_runtime_access() {
    let valid = png_bytes(512 * 1024);
    let mut bad_crc = valid.clone();
    let last = bad_crc.len() - 1;
    bad_crc[last] ^= 1;
    let truncated_idat = truncate_idat_data(valid.clone());
    let missing_iend = valid[..valid.len() - 12].to_vec();

    for payload in [bad_crc, truncated_idat, missing_iend] {
        let root = staging_root();
        let mut request = observation_request(&[512 * 1024], root.path());
        replace_observation_payload(&mut request, &payload);
        let result = response(&execute(&request));
        assert_eq!(
            result["error"]["code"], "invalid_request",
            "malformed PNG must be rejected before runtime access: {result}"
        );
    }
}
