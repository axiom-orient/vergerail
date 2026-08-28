//! Black-box stdin/stdout contract tests for the UpAgent provider process.

use serde_json::{Value, json};
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

const PROVIDER: &str = env!("CARGO_BIN_EXE_vergerail_provider");

fn model_request(reasoning: &str, maximum_response_bytes: usize) -> Value {
    json!({
        "schemaVersion": 1,
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
        "schemaVersion": 1,
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

fn execute(value: &Value) -> Output {
    let mut child = Command::new(PROVIDER)
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
    assert_eq!(result["schemaVersion"], 1);
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
