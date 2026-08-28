//! Black-box stdin/stdout contract tests for the IFSC text-provider executable.

use serde_json::{Value, json};
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

const PROVIDER: &str = env!("CARGO_BIN_EXE_ifsc_text_provider");

fn valid_request() -> Value {
    json!({
        "schemaVersion": 1,
        "operation": "screen-program-proposal",
        "idempotencyKey": "protocol-test-0001",
        "prompt": "Create a calm product screen.",
        "promptAst": {
            "screenId": "screen.home",
            "stateId": "default",
            "viewport": {
                "width": 1440,
                "height": 1024,
                "deviceScaleFactor": 1
            },
            "imageProfile": {
                "requiredSections": [
                    { "elementId": "hero", "kind": "landmark" },
                    { "elementId": "hero-title", "kind": "heading" },
                    { "elementId": "start-project", "kind": "button" }
                ],
                "copyContract": {
                    "mode": "no-readable-copy",
                    "exactText": []
                }
            }
        },
        "output": { "width": 1440, "height": 1024, "format": "png" },
        "constraints": {
            "schemaVersion": 1,
            "staticOnly": true,
            "maximumNodes": 128,
            "maximumProgramBytes": 262144,
            "exactViewport": true,
            "externalResources": false
        }
    })
}

fn execute(input: &[u8]) -> Output {
    let mut child = Command::new(PROVIDER)
        .env_remove("VERGERAIL_WORKSPACE")
        .env_remove("VERGERAIL_CODEX_PACKAGE")
        .env_remove("VERGERAIL_MODEL")
        .env_remove("VERGERAIL_IFSC_RUNTIME_DOWNLOAD")
        .env_remove("VERGERAIL_IFSC_TURN_TIMEOUT_MS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("provider process must start");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("request input");
    child.wait_with_output().expect("provider output")
}

fn typed_error(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stderr.is_empty(),
        "protocol failures must not write stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "stdout must contain exactly one JSON value");
    serde_json::from_slice(lines[0]).expect("typed JSON response")
}

#[test]
fn empty_and_malformed_inputs_fail_without_runtime_access() {
    let empty = typed_error(&execute(b""));
    assert_eq!(empty["schemaVersion"], 1);
    assert_eq!(empty["error"]["code"], "invalid-request");
    assert_eq!(empty["error"]["retryable"], false);

    let malformed = typed_error(&execute(br#"{"schemaVersion":1"#));
    assert_eq!(malformed["error"]["code"], "invalid-request");
}

#[test]
fn unknown_fields_and_weakened_constraints_are_rejected() {
    let mut unknown = valid_request();
    unknown["shell"] = json!("/bin/zsh");
    let response = typed_error(&execute(
        &serde_json::to_vec(&unknown).expect("unknown-field request"),
    ));
    assert_eq!(response["error"]["code"], "invalid-request");

    let mut weakened = valid_request();
    weakened["constraints"]["externalResources"] = json!(true);
    let response = typed_error(&execute(
        &serde_json::to_vec(&weakened).expect("weakened request"),
    ));
    assert_eq!(response["error"]["code"], "invalid-request");
    assert_eq!(response["error"]["requestId"], "protocol-test-0001");
}

#[test]
fn oversized_input_is_rejected_before_json_parsing() {
    let output = execute(&vec![b' '; 512 * 1024 + 1]);
    let response = typed_error(&output);
    assert_eq!(response["error"]["code"], "request-too-large");
}

#[test]
fn invalid_oversized_request_id_is_not_reflected() {
    let mut request = valid_request();
    request["idempotencyKey"] = json!("x".repeat(300_000));
    let output = execute(&serde_json::to_vec(&request).expect("oversized id request"));
    let response = typed_error(&output);
    assert_eq!(response["error"]["code"], "invalid-request");
    assert!(response["error"].get("requestId").is_none());
    assert!(output.stdout.len() < 4096);
}

#[test]
fn valid_request_reports_missing_runtime_configuration() {
    let response = typed_error(&execute(
        &serde_json::to_vec(&valid_request()).expect("valid request"),
    ));
    assert_eq!(response["error"]["code"], "configuration-invalid");
    assert_eq!(response["error"]["requestId"], "protocol-test-0001");
    assert_eq!(response["error"]["retryable"], false);
}
