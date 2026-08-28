//! End-to-end process and bidirectional JSON-RPC contract test.

#![cfg(unix)]

use crate::runtime::{RuntimeArtifact, RuntimeLock, RuntimePackage};
use crate::{
    Account, ApprovalEvent, Codex, CodexConfig, CommandDecision, DirectImageRequest, Event,
    ImageBackground, ImageQuality, ImageSize, LoginMethod, ReasoningEffort, SessionOptions,
    TurnStatus,
};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::thread;
use std::time::Duration;

const SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import os
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

pending_turn = None
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        assert "CODEX_HOME" not in os.environ
        send({"id": request_id, "result": {"userAgent": "fake"}})
        send({"method": "configWarning", "params": {"summary": "fake config warning"}})
        send({"id": 901, "method": "item/tool/call", "params": {"threadId": "missing"}})
    elif method == "initialized":
        pass
    elif method == "account/read":
        send({"id": request_id, "result": {"account": None, "requiresOpenaiAuth": True}})
    elif method == "account/login/start":
        send({"id": request_id, "result": {
            "type": "chatgptDeviceCode",
            "loginId": "login-1",
            "verificationUrl": "https://example.invalid/device",
            "userCode": "ABCD-EFGH"
        }})
        send({"method": "account/login/completed", "params": {
            "loginId": "login-1", "success": True, "error": None
        }})
    elif method == "account/login/cancel":
        send({"id": request_id, "result": {"status": "notFound"}})
    elif method == "account/logout":
        send({"id": request_id, "result": {}})
    elif method == "model/list":
        send({"id": request_id, "result": {"data": [{
            "id": "model-1", "model": "codex-test", "displayName": "Codex Test",
            "description": "fake model", "hidden": False, "isDefault": True
        }], "nextCursor": None}})
    elif method == "thread/start":
        assert message["params"]["sandbox"] == "read-only"
        assert message["params"]["approvalPolicy"] == "never"
        assert message["params"]["ephemeral"] is True
        send({"id": request_id, "result": {"thread": {"id": "thread-1"}}})
    elif method == "thread/resume":
        send({"id": request_id, "result": {"thread": {"id": message["params"]["threadId"]}}})
    elif method == "turn/start":
        assert message["params"]["approvalPolicy"] == "never"
        assert message["params"]["sandboxPolicy"] == {"type": "readOnly", "networkAccess": False}
        assert message["params"]["effort"] == "medium"
        pending_turn = (message["params"]["threadId"], "turn-1")
        if pending_turn[0] == "thread-existing":
            send({"method": "turn/started", "params": {
                "threadId": pending_turn[0], "turn": {"id": "turn-stale"}
            }})
            send({"method": "turn/completed", "params": {
                "threadId": pending_turn[0],
                "turn": {"id": "turn-stale", "status": "completed", "error": None}
            }})
        send({"method": "turn/started", "params": {
            "threadId": pending_turn[0], "turn": {"id": pending_turn[1]}
        }})
        send({"id": request_id, "result": {"turn": {"id": pending_turn[1]}}})
        send({"id": 900, "method": "item/commandExecution/requestApproval", "params": {
            "threadId": pending_turn[0], "turnId": pending_turn[1], "itemId": "item-1",
            "startedAtMs": 1, "environmentId": None, "command": "echo OK",
            "cwd": "/tmp", "reason": "contract test"
        }})
    elif method == "turn/interrupt":
        send({"id": request_id, "result": {}})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id == 900 and "result" in message:
        assert message["result"]["decision"] == "decline"
        thread_id, turn_id = pending_turn
        send({"method": "item/agentMessage/delta", "params": {
            "threadId": thread_id, "turnId": turn_id, "itemId": "agent-1", "delta": "OK"
        }})
        send({"method": "thread/tokenUsage/updated", "params": {
            "threadId": thread_id, "turnId": turn_id,
            "tokenUsage": {"total": {"totalTokens": 3, "inputTokens": 2,
                "cachedInputTokens": 0, "outputTokens": 1, "reasoningOutputTokens": 0},
                "last": {"totalTokens": 3, "inputTokens": 2, "cachedInputTokens": 0,
                "outputTokens": 1, "reasoningOutputTokens": 0}, "modelContextWindow": 1000}
        }})
        send({"method": "turn/completed", "params": {
            "threadId": thread_id, "turn": {"id": turn_id, "status": "completed", "error": None}
        }})
    elif request_id == 901 and "error" in message:
        assert message["error"]["code"] == -32601
        send({"method": "warning", "params": {"message": "unsupported reverse request rejected"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported fake method"}})
"###;

const IMAGE_AUTH_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

auth_count = 0
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {"userAgent": "image-auth-fixture"}})
    elif method == "initialized":
        pass
    elif method == "getAuthStatus":
        params = message["params"]
        assert params["includeToken"] is True
        assert params["refreshToken"] is True
        auth_count += 1
        assert auth_count <= 2
        payload = (
            "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoi"
            + ("Zmlyc3QtYWNjb3VudCJ9fQ" if auth_count == 1 else "c2Vjb25kLWFjY291bnQifX0")
        )
        send({"id": request_id, "result": {
            "authMethod": "chatgpt",
            "authToken": "e30." + payload + ".fixture",
            "requiresOpenaiAuth": True
        }})
    else:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const LOGIN_TIMEOUT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "account/login/start":
        send({"id": request_id, "result": {
            "type": "chatgptDeviceCode",
            "loginId": "login-timeout",
            "verificationUrl": "https://example.invalid/device",
            "userCode": "TIME-OUT"
        }})
    elif method == "account/login/cancel":
        assert message["params"]["loginId"] == "login-timeout"
        send({"id": request_id, "result": {"status": "canceled"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const LOGIN_DISCONNECT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "account/login/start":
        send({"id": request_id, "result": {
            "type": "chatgptDeviceCode",
            "loginId": "login-disconnected",
            "verificationUrl": "https://example.invalid/device",
            "userCode": "DISC-ONNECT"
        }})
        raise SystemExit(0)
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const MALFORMED_LOGIN_COMPLETION_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "account/login/start":
        send({"id": request_id, "result": {
            "type": "chatgptDeviceCode",
            "loginId": "login-malformed",
            "verificationUrl": "https://example.invalid/device",
            "userCode": "MAL-FORM"
        }})
        send({"method": "account/login/completed", "params": {
            "loginId": None, "success": True, "error": None
        }})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const RUN_AND_CLEANUP_FAILURE_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

unsubscribe_attempts = 0
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-cleanup"}}})
    elif method == "turn/start":
        send({"id": request_id, "result": {"turn": {"id": "turn-cleanup"}}})
        send({"method": "turn/started", "params": {
            "threadId": "thread-cleanup", "turn": {"id": "turn-cleanup"}
        }})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-cleanup",
            "turn": {"id": "turn-cleanup", "status": "failed",
                "error": {"message": "primary turn failure"}}
        }})
    elif method == "thread/unsubscribe":
        unsubscribe_attempts += 1
        if unsubscribe_attempts == 1:
            send({"id": request_id, "error": {
                "code": -32001, "message": "cleanup unsubscribe failure"
            }})
        else:
            send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const INTERRUPT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-interrupt"}}})
    elif method == "turn/start":
        send({"method": "turn/started", "params": {
            "threadId": "thread-interrupt", "turn": {"id": "turn-interrupt"}
        }})
        send({"id": request_id, "result": {"turn": {"id": "turn-interrupt"}}})
    elif method == "turn/interrupt":
        assert message["params"] == {
            "threadId": "thread-interrupt", "turnId": "turn-interrupt"
        }
        send({"id": request_id, "result": {}})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-interrupt",
            "turn": {"id": "turn-interrupt", "status": "interrupted", "error": None}
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const REPEATED_INTERRUPT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
import threading

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

write_lock = threading.Lock()
interrupt_count = 0

def send(value):
    with write_lock:
        sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
        sys.stdout.flush()

def finish_first_interrupt(_request_id):
    # The provider terminal notification is authoritative. Deliberately leave
    # the interrupt response pending to reproduce a stuck-response failure
    # edge without blocking either Vergerail interrupt caller.
    send({"method": "turn/completed", "params": {
        "threadId": "thread-single-flight",
        "turn": {"id": "turn-single-flight", "status": "interrupted", "error": None}
    }})

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-single-flight"}}})
    elif method == "turn/start":
        send({"method": "turn/started", "params": {
            "threadId": "thread-single-flight", "turn": {"id": "turn-single-flight"}
        }})
        send({"id": request_id, "result": {"turn": {"id": "turn-single-flight"}}})
    elif method == "turn/interrupt":
        interrupt_count += 1
        if interrupt_count == 1:
            send({"method": "configWarning", "params": {
                "summary": "single interrupt request observed"
            }})
            timer = threading.Timer(0.1, finish_first_interrupt, args=(request_id,))
            timer.start()
        else:
            send({"method": "configWarning", "params": {
                "summary": "duplicate interrupt request observed"
            }})
            # Deliberately never answer to reproduce the wedged request.
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const UNSUBSCRIBE_RETRY_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

unsubscribe_attempts = 0
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-retry"}}})
    elif method == "thread/unsubscribe":
        unsubscribe_attempts += 1
        if unsubscribe_attempts == 1:
            send({"id": request_id, "error": {"code": -32000, "message": "transient unsubscribe failure"}})
        else:
            send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported fake method"}})
"###;

const TURN_AUDIT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        mode = params.get("model") or ("ephemeral" if params["ephemeral"] else "audit")
        send({"id": request_id, "result": {"thread": {"id": "thread-" + mode}}})
    elif method == "turn/start":
        thread_id = params["threadId"]
        turn_id = "turn-active" if thread_id == "thread-active" else "turn-audit"
        send({"method": "turn/started", "params": {
            "threadId": thread_id, "turn": {"id": turn_id}
        }})
        send({"id": request_id, "result": {"turn": {"id": turn_id}}})
        if thread_id == "thread-audit":
            # No item/started or item/completed notifications are emitted. The
            # persisted audit is the only source of command/file evidence.
            send({"method": "item/agentMessage/delta", "params": {
                "threadId": thread_id, "turnId": turn_id,
                "itemId": "agent-live", "delta": "done"
            }})
            send({"method": "turn/completed", "params": {
                "threadId": thread_id,
                "turn": {"id": turn_id, "status": "completed", "error": None}
            }})
    elif method == "turn/interrupt":
        assert params == {"threadId": "thread-active", "turnId": "turn-active"}
        send({"id": request_id, "result": {}})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-active",
            "turn": {"id": "turn-active", "status": "interrupted", "error": None}
        }})
    elif method == "thread/read":
        assert params == {"threadId": params["threadId"], "includeTurns": True}
        thread_id = params["threadId"]
        if thread_id == "thread-ephemeral":
            raise AssertionError("ephemeral audit reached the wire")
        if thread_id == "thread-audit":
            thread = {"id": thread_id, "turns": [{
                "id": "turn-audit", "status": "completed", "itemsView": "full", "items": [
                    {"type": "agentMessage", "id": "agent-1"},
                    {"type": "commandExecution", "id": "command-1",
                     "command": "touch forbidden", "cwd": "/tmp/project", "status": "failed"},
                    {"type": "fileChange", "id": "patch-1", "status": "completed",
                     "changes": [{"path": "src/lib.rs"}]},
                    {"type": "reasoning", "id": "reasoning-1"}
                ]
            }]}
        elif thread_id == "thread-partial":
            thread = {"id": thread_id, "turns": [{
                "id": "turn-partial", "status": "completed", "itemsView": "summary", "items": []
            }]}
        elif thread_id == "thread-wrong":
            thread = {"id": "thread-other", "turns": []}
        elif thread_id == "thread-malformed":
            thread = {"id": thread_id, "turns": [{
                "id": "turn-malformed", "status": "completed", "itemsView": "full"
            }]}
        elif thread_id in ("thread-in-progress", "thread-interrupted", "thread-failed"):
            mode = thread_id.removeprefix("thread-")
            status = {
                "in-progress": "inProgress",
                "interrupted": "interrupted",
                "failed": "failed",
            }[mode]
            thread = {"id": thread_id, "turns": [{
                "id": "turn-" + mode, "status": status, "itemsView": "full", "items": []
            }]}
        elif thread_id == "thread-missing-status":
            thread = {"id": thread_id, "turns": [{
                "id": "turn-missing-status", "itemsView": "full", "items": []
            }]}
        else:
            raise AssertionError("unexpected thread/read")
        send({"id": request_id, "result": {"thread": thread}})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

fn read_image_http_request(mut stream: &TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set image request read timeout");
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).expect("image request headers");
        assert!(count > 0, "image request must contain headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(
            bytes.len() < 512 * 1024,
            "image request headers are bounded"
        );
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    assert!(
        content_length <= 256 * 1024,
        "image request body is bounded"
    );
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("image request body");
        assert!(count > 0, "image request body must complete");
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes
}

fn image_http_header(request: &[u8], name: &str) -> Option<String> {
    String::from_utf8_lossy(request).lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_generation_exercises_app_server_auth_refresh_and_http_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("local image fixture");
    listener
        .set_nonblocking(true)
        .expect("nonblocking image fixture");
    let endpoint = format!(
        "http://{}/backend-api/codex/images/generations",
        listener.local_addr().expect("fixture address")
    );
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let started = std::time::Instant::now();
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            started.elapsed() < Duration::from_secs(5),
                            "image endpoint fixture did not receive the expected retry"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("image endpoint fixture accept failed: {error}"),
                }
            };
            requests.push(read_image_http_request(&stream));
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("401 response");
        }
        requests
    });

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let home_directory = tempfile::tempdir().expect("home tempdir");
    let package = create_fake_package(package_directory.path(), IMAGE_AUTH_SCRIPT);
    let config = CodexConfig::new(package)
        .with_codex_home(home_directory.path())
        .expect("explicit auth home")
        .with_image_generation(true)
        .with_request_timeout(Duration::from_secs(3));
    let mut codex = Codex::connect(config)
        .await
        .expect("connect fixture app-server");
    codex.set_image_endpoint_for_test(endpoint);

    let error = codex
        .generate_image(DirectImageRequest {
            model: "gpt-image-1".to_owned(),
            prompt: "fixture image".to_owned(),
            background: ImageBackground::Transparent,
            size: ImageSize::Square,
            quality: ImageQuality::Low,
        })
        .await
        .expect_err("second 401 must terminate the image operation");
    assert_eq!(error.kind(), crate::ErrorKind::Authentication);
    codex.shutdown().await.expect("shutdown fixture app-server");

    let requests = server.join().expect("image endpoint fixture thread");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        image_http_header(&requests[0], "Chatgpt-Account-Id").as_deref(),
        Some("first-account")
    );
    assert_eq!(
        image_http_header(&requests[1], "Chatgpt-Account-Id").as_deref(),
        Some("second-account")
    );
    let first_turn = image_http_header(&requests[0], "x-codex-image-turn-id");
    let second_turn = image_http_header(&requests[1], "x-codex-image-turn-id");
    assert!(first_turn.as_deref().is_some_and(|value| !value.is_empty()));
    assert_eq!(first_turn, second_turn);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_contract_uses_real_process_and_bidirectional_rpc() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), SCRIPT);

    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    assert_eq!(
        codex.account().await.expect("account"),
        Account::SignedOut {
            requires_openai_auth: true
        }
    );

    let models = codex.models().await.expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model(), "codex-test");

    let login = codex
        .login(LoginMethod::DeviceCode)
        .await
        .expect("start login");
    assert_eq!(login.user_code(), Some("ABCD-EFGH"));
    assert!(matches!(
        login.wait().await.expect("login completion"),
        Account::SignedOut { .. }
    ));
    login
        .cancel()
        .await
        .expect("cancel after terminal success is idempotent");
    assert!(matches!(
        login
            .wait()
            .await
            .expect("first terminal login result must win the cancel race"),
        Account::SignedOut { .. }
    ));

    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");
    let mut run = session.start("Reply with OK").await.expect("run");
    let mut saw_started = false;
    let mut saw_delta = false;
    let result = loop {
        match run
            .next_event()
            .await
            .expect("event stream")
            .expect("event")
        {
            Event::Started => saw_started = true,
            Event::ApprovalRequested(ApprovalEvent::Command(request)) => {
                assert_eq!(request.command.as_deref(), Some("echo OK"));
                request
                    .respond(CommandDecision::Decline)
                    .await
                    .expect("decline");
            }
            Event::TextDelta(delta) => {
                assert_eq!(delta, "OK");
                saw_delta = true;
            }
            Event::Completed(result) => break result,
            Event::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    };
    assert!(saw_started);
    assert!(saw_delta);
    assert_eq!(result.text, "OK");
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(result.usage.expect("usage").total_tokens, 3);

    session.close().await.expect("unsubscribe");
    let diagnostics = codex.take_diagnostics().await;
    assert!(
        diagnostics
            .iter()
            .any(|item| item.method == "configWarning")
    );
    assert!(
        diagnostics
            .iter()
            .any(|item| item.message == "unsupported reverse request rejected")
    );
    codex.logout().await.expect("logout");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_codex_home_is_removed_before_app_server_spawn() {
    const CHILD_MARKER: &str = "VERGERAIL_AUTH_ENV_REMOVAL_CHILD";

    if std::env::var_os(CHILD_MARKER).is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("contract_tests::inherited_codex_home_is_removed_before_app_server_spawn")
            .arg("--test-threads=1")
            .env(CHILD_MARKER, "1")
            .env("CODEX_HOME", "/vergerail-test-sentinel")
            .status()
            .expect("isolated contract subprocess");
        assert!(status.success(), "isolated contract subprocess failed");
        return;
    }

    assert_eq!(
        std::env::var("CODEX_HOME").as_deref(),
        Ok("/vergerail-test-sentinel")
    );
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_fake_package(package_directory.path(), SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("child CODEX_HOME must be removed");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_codex_home_is_forwarded_to_app_server_spawn() {
    let home_directory = tempfile::tempdir().expect("home tempdir");
    let expected_home = serde_json::to_string(
        home_directory
            .path()
            .to_str()
            .expect("home path must be UTF-8"),
    )
    .expect("home JSON string");
    let script = SCRIPT.replace(
        "assert \"CODEX_HOME\" not in os.environ",
        &format!("assert os.environ.get(\"CODEX_HOME\") == {expected_home}"),
    );
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_fake_package(package_directory.path(), &script);
    let config = CodexConfig::new(package)
        .with_codex_home(home_directory.path())
        .expect("explicit home");
    let codex = Codex::connect(config)
        .await
        .expect("explicit Codex home must reach the child app-server");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_output_schema_and_extended_reasoning_reach_turn_start() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["text"],
        "properties": {"text": {"type": "string"}}
    });
    let script = SCRIPT.replace(
        "assert message[\"params\"][\"effort\"] == \"medium\"",
        concat!(
            "assert message[\"params\"][\"effort\"] in [\"xhigh\", \"max\"]\n",
            "        assert message[\"params\"][\"outputSchema\"] == {",
            "\"type\":\"object\",\"additionalProperties\":False,",
            "\"required\":[\"text\"],\"properties\":{\"text\":{\"type\":\"string\"}}}"
        ),
    );
    assert_ne!(script, SCRIPT, "turn/start assertion must be active");
    let package = create_fake_package(package_directory.path(), &script);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    for effort in [ReasoningEffort::XHigh, ReasoningEffort::Max] {
        let result = codex
            .run(
                "return structured output",
                SessionOptions::read_only(project_directory.path())
                    .with_reasoning(effort)
                    .with_output_schema(schema.clone()),
            )
            .await
            .expect("structured turn");
        assert_eq!(result.status, TurnStatus::Completed);
    }
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_only_thread_enables_no_other_capability() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let script = SCRIPT.replace(
        "elif method == \"thread/start\":",
        concat!(
            "elif method == \"thread/start\":\n",
            "        config = message[\"params\"][\"config\"]\n",
            "        assert config[\"features\"][\"image_generation\"] is True\n",
            "        assert config[\"features\"][\"shell_tool\"] is False\n",
            "        assert config[\"features\"][\"unified_exec\"] is False\n",
            "        assert config[\"web_search\"] == \"disabled\"\n",
            "        assert config[\"history\"][\"persistence\"] == \"none\""
        ),
    );
    assert_ne!(
        script, SCRIPT,
        "thread/start capability assertion must be active"
    );
    let package = create_fake_package(package_directory.path(), &script);
    let codex = Codex::connect(CodexConfig::new(package).with_image_generation(true))
        .await
        .expect("connect");
    let session = codex
        .session(
            SessionOptions::read_only(project_directory.path())
                .image_only()
                .ephemeral(),
        )
        .await
        .expect("image-only session");
    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_generation_session_sends_low_reasoning_effort() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let low_effort_script = SCRIPT.replace(
        "assert message[\"params\"][\"effort\"] == \"medium\"",
        "assert message[\"params\"][\"effort\"] == \"low\"",
    );
    assert_ne!(
        low_effort_script, SCRIPT,
        "the low-effort contract must be active"
    );
    let package = create_fake_package(package_directory.path(), &low_effort_script);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let result = codex
        .run(
            "Generate exactly one image.",
            SessionOptions::read_only(project_directory.path())
                .with_model("gpt-5.6-luna")
                .with_reasoning(ReasoningEffort::Low),
        )
        .await
        .expect("image-generation run");
    assert_eq!(result.status, TurnStatus::Completed);
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_interrupt_dispatches_exact_ids_and_reaches_terminal_state() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), INTERRUPT_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");
    let run = session.start("wait for interruption").await.expect("run");
    run.interrupt().await.expect("interrupt");
    let result = run.wait().await.expect("interrupted terminal result");

    assert_eq!(result.thread_id, "thread-interrupt");
    assert_eq!(result.turn_id, "turn-interrupt");
    assert_eq!(result.status, TurnStatus::Interrupted);
    session.close().await.expect("unsubscribe");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_interrupt_callers_share_one_provider_request() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), REPEATED_INTERRUPT_SCRIPT);
    let config = CodexConfig::new(package).with_request_timeout(Duration::from_secs(5));
    let codex = Codex::connect(config).await.expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");
    let run = session.start("interrupt once").await.expect("run");

    let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(run.interrupt(), run.interrupt())
    })
    .await
    .expect("a duplicate provider interrupt would leave one caller pending");
    first.expect("first interrupt caller");
    second.expect("second interrupt caller");

    let result = run.wait().await.expect("interrupted terminal result");
    assert_eq!(result.status, TurnStatus::Interrupted);
    let diagnostics = codex.take_diagnostics().await;
    assert!(
        diagnostics
            .iter()
            .any(|item| item.message == "single interrupt request observed")
    );
    assert!(
        diagnostics
            .iter()
            .all(|item| item.message != "duplicate interrupt request observed")
    );

    session.close().await.expect("unsubscribe");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_run_auto_denies_approvals_and_cleans_up_the_ephemeral_session() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let result = codex
        .run(
            "Reply with OK",
            SessionOptions::read_only(project_directory.path()),
        )
        .await
        .expect("one-shot run");

    assert_eq!(result.text, "OK");
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(result.usage.expect("usage").total_tokens, 3);
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_login_completion_is_connection_fatal_instead_of_timing_out() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_fake_package(package_directory.path(), MALFORMED_LOGIN_COMPLETION_SCRIPT);
    let codex =
        Codex::connect(CodexConfig::new(package).with_login_timeout(Duration::from_secs(5)))
            .await
            .expect("connect");

    let login = codex
        .login(LoginMethod::DeviceCode)
        .await
        .expect("valid start response still yields a tracked handle");
    let error = login
        .wait()
        .await
        .expect_err("unroutable completion must fail immediately");
    assert_eq!(error.kind(), crate::ErrorKind::Protocol);
    assert_eq!(error.operation(), "account.login.completed");
    assert!(error.message().contains("loginId"));

    let subsequent = codex
        .account()
        .await
        .expect_err("the malformed pinned notification terminates the connection");
    assert_eq!(subsequent.kind(), crate::ErrorKind::Protocol);
    codex
        .shutdown()
        .await
        .expect("shutdown after forced disconnect");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_run_preserves_primary_failure_when_cleanup_also_fails() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), RUN_AND_CLEANUP_FAILURE_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let error = codex
        .run(
            "fail the turn",
            SessionOptions::read_only(project_directory.path()),
        )
        .await
        .expect_err("run and cleanup are both expected to fail");

    assert_eq!(error.kind(), crate::ErrorKind::Rpc);
    assert_eq!(error.operation(), "turn.completed");
    assert!(error.message().contains("primary turn failure"));
    assert!(
        error
            .message()
            .contains("ephemeral session cleanup also failed")
    );
    assert!(error.message().contains("cleanup unsubscribe failure"));

    codex
        .shutdown()
        .await
        .expect("shutdown remains clean after bounded one-shot cleanup recovery");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_thread_can_be_resumed_run_and_unsubscribed() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let session = codex
        .resume(
            "thread-existing",
            SessionOptions::read_only(project_directory.path()),
        )
        .await
        .expect("resume session");
    assert_eq!(session.id(), "thread-existing");

    let result = session
        .start("Reply with OK")
        .await
        .expect("run")
        .wait()
        .await
        .expect("completion");
    assert_eq!(result.text, "OK");
    assert_eq!(result.status, TurnStatus::Completed);

    session.close().await.expect("unsubscribe");
    assert!(
        codex
            .take_diagnostics()
            .await
            .iter()
            .any(|item| item.method == "rpc/staleTurnNotification")
    );
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_timeout_keeps_handle_cancelable_and_records_terminal_cancellation() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_fake_package(package_directory.path(), LOGIN_TIMEOUT_SCRIPT);
    let mut config = CodexConfig::new(package);
    config.login_timeout = Duration::from_millis(100);
    let codex = Codex::connect(config).await.expect("connect");

    let login = codex
        .login(LoginMethod::DeviceCode)
        .await
        .expect("start login");
    let timeout = login.wait().await.expect_err("login must time out");
    assert_eq!(timeout.kind(), crate::ErrorKind::Timeout);

    login.cancel().await.expect("cancel after timeout");
    let canceled = login
        .wait()
        .await
        .expect_err("canceled login must remain terminal");
    assert_eq!(canceled.kind(), crate::ErrorKind::Authentication);
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_response_followed_by_disconnect_is_retained_as_terminal_failure() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_fake_package(package_directory.path(), LOGIN_DISCONNECT_SCRIPT);
    let config = CodexConfig::new(package).with_login_timeout(Duration::from_secs(5));
    let codex = Codex::connect(config).await.expect("connect");
    let login = codex
        .login(LoginMethod::DeviceCode)
        .await
        .expect("login response is delivered before process exit");

    let error = tokio::time::timeout(Duration::from_secs(1), login.wait())
        .await
        .expect("disconnect must be observable without waiting for the login timeout")
        .expect_err("disconnected login cannot succeed");

    assert_eq!(error.kind(), crate::ErrorKind::Disconnected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribe_failure_preserves_session_for_retry() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), UNSUBSCRIBE_RETRY_SCRIPT);

    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");

    let error = session
        .close()
        .await
        .expect_err("first unsubscribe must fail");
    assert_eq!(error.kind(), crate::ErrorKind::Rpc);
    assert_eq!(error.rpc_code(), Some(-32000));

    session
        .close()
        .await
        .expect("failed close must leave session retryable");
    session.close().await.expect("closed session is idempotent");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_turn_audit_recovers_items_omitted_from_live_notifications() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), TURN_AUDIT_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()))
        .await
        .expect("persistent session");
    let mut run = session.start("perform audited work").await.expect("run");
    let mut saw_live_command_or_file = false;
    let result = loop {
        match run
            .next_event()
            .await
            .expect("event stream")
            .expect("event")
        {
            Event::Command(_) | Event::FileChange(_) => saw_live_command_or_file = true,
            Event::Completed(result) => break result,
            Event::Failed(error) => panic!("unexpected run failure: {error}"),
            _ => {}
        }
    };
    assert!(!saw_live_command_or_file);

    let audit = session
        .audit_turn(&result.turn_id)
        .await
        .expect("durable turn audit");
    assert_eq!(audit.turn_id, result.turn_id);
    assert_eq!(audit.commands.len(), 1);
    assert_eq!(audit.commands[0].command, "touch forbidden");
    assert_eq!(audit.commands[0].status, "failed");
    assert_eq!(audit.file_changes.len(), 1);
    assert_eq!(audit.file_changes[0].paths, [Path::new("src/lib.rs")]);
    assert_eq!(audit.other_item_types, ["agentMessage", "reasoning"]);

    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_audit_rejects_partial_wrong_and_malformed_runtime_history() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), TURN_AUDIT_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    for (mode, turn_id, expected_message) in [
        ("partial", "turn-partial", "partial"),
        ("wrong", "turn-wrong", "thread-other"),
        ("malformed", "turn-malformed", "items"),
        ("in-progress", "turn-in-progress", "not completed"),
        ("interrupted", "turn-interrupted", "not completed"),
        ("failed", "turn-failed", "not completed"),
        ("missing-status", "turn-missing-status", "status"),
    ] {
        let session = codex
            .session(SessionOptions::read_only(project_directory.path()).with_model(mode))
            .await
            .expect("session");
        let error = session
            .audit_turn(turn_id)
            .await
            .expect_err("invalid history must fail closed");
        assert_eq!(error.kind(), crate::ErrorKind::Protocol);
        assert!(error.message().contains(expected_message), "{error}");
        session.close().await.expect("close after read failure");
    }

    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_audit_rejects_invalid_session_states_without_a_read_request() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), TURN_AUDIT_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let ephemeral = codex
        .session(
            SessionOptions::read_only(project_directory.path())
                .ephemeral()
                .with_model("ephemeral"),
        )
        .await
        .expect("ephemeral session");
    let empty_prompt = ephemeral.start(" ").await.expect_err("empty prompt");
    assert_eq!(empty_prompt.kind(), crate::ErrorKind::InvalidInput);
    let empty = ephemeral.audit_turn(" ").await.expect_err("empty turn id");
    assert_eq!(empty.kind(), crate::ErrorKind::InvalidInput);
    let ephemeral_error = ephemeral
        .audit_turn("turn-ephemeral")
        .await
        .expect_err("ephemeral history is unavailable");
    assert_eq!(ephemeral_error.kind(), crate::ErrorKind::InvalidInput);
    ephemeral.close().await.expect("close ephemeral");

    let active = codex
        .session(SessionOptions::read_only(project_directory.path()).with_model("active"))
        .await
        .expect("active session");
    let run = active.start("wait").await.expect("active run");
    let active_error = active
        .audit_turn("turn-active")
        .await
        .expect_err("active session cannot be audited");
    assert_eq!(active_error.kind(), crate::ErrorKind::InvalidInput);
    run.interrupt().await.expect("interrupt");
    assert_eq!(
        run.wait().await.expect("terminal").status,
        TurnStatus::Interrupted
    );
    active.close().await.expect("close active session");
    let closed_error = active
        .audit_turn("turn-active")
        .await
        .expect_err("closed session cannot be audited");
    assert_eq!(closed_error.kind(), crate::ErrorKind::InvalidInput);

    codex.shutdown().await.expect("shutdown");
}

fn create_fake_package(root: &Path, script: &str) -> RuntimePackage {
    let entrypoint = root.join("bin/codex");
    let app = root.join("bin/app.py");
    fs::create_dir_all(entrypoint.parent().expect("bin parent")).expect("bin");
    fs::create_dir_all(root.join("codex-path")).expect("path dir");
    fs::create_dir_all(root.join("codex-resources")).expect("resources dir");
    fs::write(
        &entrypoint,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.test'; exit 0; fi\nexec python3 \"$(dirname \"$0\")/app.py\" \"$@\"\n",
    )
    .expect("entrypoint");
    fs::write(&app, script).expect("app script");
    let mut permissions = fs::metadata(&entrypoint).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&entrypoint, permissions).expect("executable");

    let manifest_path = root.join("codex-package.json");
    fs::write(
        &manifest_path,
        r#"{"layoutVersion":1,"version":"0.test","target":"test-target","variant":"codex","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path"}"#,
    )
    .expect("manifest");

    let lock = RuntimeLock::new(
        "0.test",
        "0000000000000000000000000000000000000000",
        "test-target",
        "codex",
        "bin/codex",
        "0000000000000000000000000000000000000000000000000000000000000000",
        vec![
            RuntimeArtifact::new("bin/codex", hash(&entrypoint), true).expect("entry lock"),
            RuntimeArtifact::new("bin/app.py", hash(&app), false).expect("app lock"),
            RuntimeArtifact::new("codex-package.json", hash(&manifest_path), false)
                .expect("manifest lock"),
        ],
    )
    .expect("runtime lock");
    RuntimePackage::new(root, lock)
}

fn with_marker_root(script: &str, root: &Path) -> String {
    let root = root.to_str().expect("marker root must be UTF-8");
    let quoted = serde_json::to_string(root).expect("marker root JSON string");
    script.replace("__MARKER_ROOT__", &quoted)
}

fn create_timeout_package(root: &Path) -> RuntimePackage {
    let entrypoint = root.join("bin/codex");
    fs::create_dir_all(entrypoint.parent().expect("bin parent")).expect("bin");
    fs::create_dir_all(root.join("codex-path")).expect("path dir");
    fs::create_dir_all(root.join("codex-resources")).expect("resources dir");
    fs::write(
        &entrypoint,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'codex-cli 0.test'
  exit 0
fi
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      tail=${line#*\"id\":}
      id=${tail%%,*}
      printf '{"id":%s,"result":{}}\n' "$id"
      ;;
    *) ;;
  esac
done
"#,
    )
    .expect("entrypoint");
    let mut permissions = fs::metadata(&entrypoint).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&entrypoint, permissions).expect("executable");

    let manifest_path = root.join("codex-package.json");
    fs::write(
        &manifest_path,
        r#"{"layoutVersion":1,"version":"0.test","target":"test-target","variant":"codex","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path"}"#,
    )
    .expect("manifest");

    let lock = RuntimeLock::new(
        "0.test",
        "0000000000000000000000000000000000000000",
        "test-target",
        "codex",
        "bin/codex",
        "0000000000000000000000000000000000000000000000000000000000000000",
        vec![
            RuntimeArtifact::new("bin/codex", hash(&entrypoint), true).expect("entry lock"),
            RuntimeArtifact::new("codex-package.json", hash(&manifest_path), false)
                .expect("manifest lock"),
        ],
    )
    .expect("runtime lock");
    RuntimePackage::new(root, lock)
}

fn hash(path: &Path) -> String {
    let bytes = fs::read(path).expect("hash read");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut output = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(output, "{byte:02x}").expect("hex");
    }
    output
}

const WORKSPACE_POLICY_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        assert params["sandbox"] == "workspace-write"
        assert params["approvalPolicy"] == "on-request"
        assert params["approvalsReviewer"] == "user"
        send({"id": request_id, "result": {"thread": {"id": "thread-write"}}})
    elif method == "turn/start":
        assert params["approvalPolicy"] == "on-request"
        policy = params["sandboxPolicy"]
        assert policy["type"] == "workspaceWrite"
        assert policy["networkAccess"] is False
        assert policy["writableRoots"] == [params["cwd"]]
        assert policy["excludeTmpdirEnvVar"] is True
        assert policy["excludeSlashTmp"] is True
        send({"id": request_id, "result": {"turn": {"id": "turn-write"}}})
        send({"method": "turn/completed", "params": {
            "threadId": params["threadId"],
            "turn": {"id": "turn-write", "status": "completed", "error": None}
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const EOF_PENDING_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import os
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
reads = 0
for raw in sys.stdin:
    message = json.loads(raw)
    if message.get("method") == "initialize":
        send({"id": message["id"], "result": {}})
    elif message.get("method") == "initialized":
        pass
    elif message.get("method") == "account/read":
        reads += 1
        if reads == 2:
            sys.stderr.write("Authorization: Bearer super-secret-token\n")
            sys.stderr.flush()
            os.close(1)
"###;

const CONCURRENT_RUNS_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
thread_count = 0
turns = []
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        thread_count += 1
        thread_id = "thread-" + str(thread_count)
        send({"id": request_id, "result": {"thread": {"id": thread_id}}})
    elif method == "turn/start":
        thread_id = params["threadId"]
        turn_id = "turn-" + thread_id.split("-")[-1]
        turns.append((thread_id, turn_id))
        send({"id": request_id, "result": {"turn": {"id": turn_id}}})
        send({"method": "turn/started", "params": {"threadId": thread_id, "turn": {"id": turn_id}}})
        if len(turns) == 2:
            first, second = turns
            for thread_id, turn_id, delta in [
                (second[0], second[1], "B1"),
                (first[0], first[1], "A1"),
                (second[0], second[1], "B2"),
                (first[0], first[1], "A2"),
            ]:
                send({"method": "item/agentMessage/delta", "params": {
                    "threadId": thread_id, "turnId": turn_id,
                    "itemId": "agent-" + turn_id, "delta": delta
                }})
            for thread_id, turn_id in [second, first]:
                send({"method": "turn/completed", "params": {
                    "threadId": thread_id,
                    "turn": {"id": turn_id, "status": "completed", "error": None}
                }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const CONSUMER_LAG_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-lag"}}})
    elif method == "turn/start":
        send({"id": request_id, "result": {"turn": {"id": "turn-lag"}}})
        send({"method": "turn/started", "params": {
            "threadId": "thread-lag", "turn": {"id": "turn-lag"}
        }})
        for index in range(16):
            send({"method": "item/agentMessage/delta", "params": {
                "threadId": "thread-lag", "turnId": "turn-lag",
                "itemId": "agent-lag", "delta": str(index)
            }})
    elif method == "turn/interrupt":
        send({"id": request_id, "result": {}})
        send({"method": "configWarning", "params": {"summary": "interrupt observed"}})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-lag",
            "turn": {"id": "turn-lag", "status": "interrupted", "error": None}
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const PRE_ACK_FAILURE_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-pre-ack"}}})
    elif method == "turn/start":
        prompt = params["input"][0]["text"]
        if prompt == "malformed":
            send({"method": "turn/completed", "params": {
                "threadId": "thread-pre-ack",
                "turn": {"id": "turn-pre-ack", "status": "unexpected", "error": None}
            }})
        else:
            for index in range(3):
                send({"method": "item/agentMessage/delta", "params": {
                    "threadId": "thread-pre-ack", "turnId": "turn-pre-ack",
                    "itemId": "agent-pre-ack", "delta": str(index)
                }})
        send({"id": request_id, "result": {"turn": {"id": "turn-pre-ack"}}})
    elif method == "account/read":
        send({"id": request_id, "result": {"account": None, "requiresOpenaiAuth": True}})
    elif method == "turn/interrupt":
        send({"id": request_id, "result": {}})
        send({"method": "configWarning", "params": {
            "summary": "pre-ack overflow interrupt observed"
        }})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-pre-ack",
            "turn": {"id": "turn-pre-ack", "status": "interrupted", "error": None}
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const PRE_ACK_TERMINAL_OVERFLOW_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-terminal-overflow"}}})
    elif method == "turn/start":
        send({"method": "item/agentMessage/delta", "params": {
            "threadId": "thread-terminal-overflow", "turnId": "turn-terminal-overflow",
            "itemId": "agent-terminal-overflow", "delta": "queued"
        }})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-terminal-overflow",
            "turn": {"id": "turn-terminal-overflow", "status": "completed", "error": None}
        }})
        send({"id": request_id, "result": {"turn": {"id": "turn-terminal-overflow"}}})
    elif method == "turn/interrupt":
        send({"id": request_id, "result": {}})
        send({"method": "configWarning", "params": {
            "summary": "unexpected interrupt after provider terminal"
        }})
    elif method == "account/read":
        send({"id": request_id, "result": {"account": None, "requiresOpenaiAuth": True}})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const START_SHUTDOWN_RACE_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
import threading

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

write_lock = threading.Lock()
turn_start_completed = threading.Event()
turn_terminal_sent = threading.Event()

def send(value):
    with write_lock:
        sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
        sys.stdout.flush()

def complete_turn_start(request_id):
    turn_start_completed.set()
    send({"id": request_id, "result": {"turn": {"id": "turn-start-shutdown"}}})

def complete_interruption():
    turn_terminal_sent.set()
    send({"method": "turn/completed", "params": {
        "threadId": "thread-start-shutdown",
        "turn": {"id": "turn-start-shutdown", "status": "interrupted", "error": None}
    }})

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-start-shutdown"}}})
    elif method == "turn/start":
        send({"method": "configWarning", "params": {
            "summary": "turn/start request observed before shutdown"
        }})
        timer = threading.Timer(0.2, complete_turn_start, args=(request_id,))
        timer.start()
    elif method == "turn/interrupt":
        send({"id": request_id, "result": {}})
        timer = threading.Timer(0.2, complete_interruption)
        timer.start()
    elif method == "thread/unsubscribe":
        if not turn_start_completed.is_set():
            send({"id": request_id, "error": {
                "code": -32010,
                "message": "thread was unsubscribed before turn/start ownership was established"
            }})
        elif not turn_terminal_sent.is_set():
            send({"id": request_id, "error": {
                "code": -32011,
                "message": "thread was unsubscribed before provider terminal completion"
            }})
        else:
            send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const CANCELLED_ONE_SHOT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import os
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

home = __MARKER_ROOT__

def mark(name):
    with open(os.path.join(home, name), "w", encoding="utf-8") as marker:
        marker.write("observed\n")

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "account/read":
        send({"id": request_id, "result": {"account": None, "requiresOpenaiAuth": True}})
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-cancelled-one-shot"}}})
    elif method == "turn/start":
        send({"method": "turn/started", "params": {
            "threadId": "thread-cancelled-one-shot",
            "turn": {"id": "turn-cancelled-one-shot"}
        }})
        send({"id": request_id, "result": {"turn": {"id": "turn-cancelled-one-shot"}}})
        mark("one-shot-turn-started")
    elif method == "turn/interrupt":
        mark("one-shot-interrupted")
        send({"id": request_id, "result": {}})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-cancelled-one-shot",
            "turn": {"id": "turn-cancelled-one-shot", "status": "interrupted", "error": None}
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
        mark("one-shot-cleaned")
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const CANCELLED_SHUTDOWN_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import os
import sys
import time

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

home = __MARKER_ROOT__

def mark(name):
    with open(os.path.join(home, name), "w", encoding="utf-8") as marker:
        marker.write("observed\n")

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-cancelled-shutdown"}}})
    elif method == "thread/unsubscribe":
        mark("shutdown-unsubscribe-observed")
        time.sleep(0.25)
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})

mark("shutdown-stdin-closed")
"###;

const MALFORMED_NOTIFICATION_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-malformed"}}})
    elif method == "turn/start":
        send({"id": request_id, "result": {"turn": {"id": "turn-malformed"}}})
        send({"method": "turn/started", "params": {
            "threadId": "thread-malformed", "turn": {"id": "turn-malformed"}
        }})
        send({"method": "item/commandExecution/outputDelta", "params": {
            "threadId": "thread-malformed", "turnId": "turn-malformed",
            "itemId": "command-malformed"
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const MALFORMED_THREAD_START_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {}}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const RUN_TIMEOUT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-timeout"}}})
    elif method == "turn/start":
        send({"id": request_id, "result": {"turn": {"id": "turn-timeout"}}})
        send({"method": "turn/started", "params": {
            "threadId": "thread-timeout", "turn": {"id": "turn-timeout"}
        }})
    elif method == "turn/interrupt":
        assert message["params"] == {
            "threadId": "thread-timeout", "turnId": "turn-timeout"
        }
        send({"id": request_id, "result": {}})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-timeout",
            "turn": {"id": "turn-timeout", "status": "interrupted", "error": None}
        }})
    elif method == "account/read":
        send({"id": request_id, "result": {
            "account": None, "requiresOpenaiAuth": True
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const RUN_OUTPUT_LIMIT_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-output"}}})
    elif method == "turn/start":
        send({"id": request_id, "result": {"turn": {"id": "turn-output"}}})
        send({"method": "item/agentMessage/delta", "params": {
            "threadId": "thread-output", "turnId": "turn-output",
            "itemId": "agent-output", "delta": "1234"
        }})
        send({"method": "item/agentMessage/delta", "params": {
            "threadId": "thread-output", "turnId": "turn-output",
            "itemId": "agent-output", "delta": "5"
        }})
    elif method == "turn/interrupt":
        send({"id": request_id, "result": {}})
        send({"method": "turn/completed", "params": {
            "threadId": "thread-output",
            "turn": {"id": "turn-output", "status": "interrupted", "error": None}
        }})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

const APPROVAL_FAIL_CLOSED_SCRIPT: &str = r###"#!/usr/bin/env python3
import json
import sys
if len(sys.argv) == 2 and sys.argv[1] == "--version":
    print("codex-cli 0.test")
    raise SystemExit(0)
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()
responses = 0
for raw in sys.stdin:
    message = json.loads(raw)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "initialized":
        pass
    elif method == "thread/start":
        send({"id": request_id, "result": {"thread": {"id": "thread-approval"}}})
    elif method == "turn/start":
        send({"id": request_id, "result": {"turn": {"id": "turn-approval"}}})
        send({"id": 800, "method": "item/commandExecution/requestApproval", "params": {
            "threadId": "thread-approval", "turnId": "turn-approval", "itemId": "item-approval",
            "startedAtMs": 1, "environmentId": None,
            "command": "echo denied", "cwd": params["cwd"], "reason": "fail closed"
        }})
    elif request_id == 800 and "result" in message:
        responses += 1
        assert message["result"] == {"decision": "decline"}
        if responses == 1:
            send({"method": "turn/completed", "params": {
                "threadId": "thread-approval",
                "turn": {"id": "turn-approval", "status": "completed", "error": None}
            }})
        else:
            send({"method": "configWarning", "params": {"summary": "duplicate approval response"}})
    elif method == "thread/unsubscribe":
        send({"id": request_id, "result": {"status": "unsubscribed"}})
    elif request_id is not None:
        send({"id": request_id, "error": {"code": -32601, "message": "unsupported"}})
"###;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_write_uses_exact_pinned_policy() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), WORKSPACE_POLICY_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(SessionOptions::workspace_write(project_directory.path()).ephemeral())
        .await
        .expect("workspace session");
    let result = session
        .start("verify policy")
        .await
        .expect("run")
        .wait()
        .await
        .expect("completion");
    assert_eq!(result.status, TurnStatus::Completed);
    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_request_timeout_is_explicit_and_runtime_remains_controllable() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_timeout_package(package_directory.path());
    let config = CodexConfig::new(package)
        .with_request_timeout(Duration::from_millis(500))
        .with_shutdown_timeout(Duration::from_secs(1));
    let codex = Codex::connect(config).await.expect("connect");
    let error = codex
        .account()
        .await
        .expect_err("account request must time out");
    assert_eq!(error.kind(), crate::ErrorKind::Timeout);
    assert_eq!(error.operation(), "account.read");
    codex
        .shutdown()
        .await
        .expect("runtime remains controllable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_turn_timeout_interrupts_and_preserves_runtime_recovery() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), RUN_TIMEOUT_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(
            SessionOptions::read_only(project_directory.path())
                .ephemeral()
                .with_turn_timeout(Duration::from_millis(50)),
        )
        .await
        .expect("session");

    let error = tokio::time::timeout(Duration::from_secs(2), async {
        session
            .start("wait for deadline")
            .await
            .expect("run handle")
            .wait()
            .await
    })
    .await
    .expect("deadline recovery must be bounded")
    .expect_err("run must time out");
    assert_eq!(error.kind(), crate::ErrorKind::Timeout);
    assert_eq!(error.operation(), "turn.run");
    assert!(matches!(
        codex.account().await.expect("runtime remains usable"),
        Account::SignedOut { .. }
    ));
    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cumulative_output_limit_interrupts_and_releases_the_session() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), RUN_OUTPUT_LIMIT_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(
            SessionOptions::read_only(project_directory.path())
                .ephemeral()
                .with_maximum_output_bytes(4),
        )
        .await
        .expect("session");

    let error = tokio::time::timeout(Duration::from_secs(2), async {
        session
            .start("bounded output")
            .await
            .expect("run handle")
            .wait()
            .await
    })
    .await
    .expect("output-limit recovery must be bounded")
    .expect_err("run must exceed the output limit");
    assert_eq!(error.kind(), crate::ErrorKind::ResourceLimit);
    assert_eq!(error.operation(), "run.output");
    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_idempotent_timeout_is_outcome_unknown_and_runtime_is_terminated() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_timeout_package(package_directory.path());
    let config = CodexConfig::new(package)
        .with_request_timeout(Duration::from_millis(500))
        .with_shutdown_timeout(Duration::from_secs(1));
    let codex = Codex::connect(config).await.expect("connect");
    let error = codex
        .session(SessionOptions::read_only(project_directory.path()))
        .await
        .expect_err("thread start outcome must be unknown");
    assert_eq!(error.kind(), crate::ErrorKind::OutcomeUnknown);
    assert_eq!(error.operation(), "thread.start");
    let disconnected = codex
        .account()
        .await
        .expect_err("a runtime terminated after an unknown outcome must reject new requests");
    assert_eq!(disconnected.kind(), crate::ErrorKind::OutcomeUnknown);
    codex
        .shutdown()
        .await
        .expect("shutdown remains idempotent after the runtime was terminated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_eof_fails_all_pending_requests_and_redacts_stderr() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let package = create_fake_package(package_directory.path(), EOF_PENDING_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let (first, second) = tokio::join!(codex.account(), codex.account());
    for error in [
        first.expect_err("first pending request"),
        second.expect_err("second pending request"),
    ] {
        assert_eq!(error.kind(), crate::ErrorKind::Disconnected);
        let stderr = error.stderr_tail().expect("stderr tail");
        assert!(stderr.contains("<redacted>"));
        assert!(!stderr.contains("super-secret-token"));
    }
    codex.shutdown().await.expect("shutdown after stdout EOF");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_route_interleaved_events_without_cross_talk() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let first_project = tempfile::tempdir().expect("first project");
    let second_project = tempfile::tempdir().expect("second project");
    let package = create_fake_package(package_directory.path(), CONCURRENT_RUNS_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let first = codex
        .session(SessionOptions::read_only(first_project.path()).ephemeral())
        .await
        .expect("first session");
    let second = codex
        .session(SessionOptions::read_only(second_project.path()).ephemeral())
        .await
        .expect("second session");
    let (first_run, second_run) = tokio::join!(first.start("first"), second.start("second"));
    let (first_result, second_result) = tokio::join!(
        first_run.expect("first run").wait(),
        second_run.expect("second run").wait()
    );
    assert_eq!(first_result.expect("first result").text, "A1A2");
    assert_eq!(second_result.expect("second result").text, "B1B2");
    let (first_close, second_close) = tokio::join!(first.close(), second.close());
    first_close.expect("first close");
    second_close.expect("second close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumer_lag_is_observable_and_interrupts_the_turn() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), CONSUMER_LAG_SCRIPT);
    let mut config = CodexConfig::new(package).with_request_timeout(Duration::from_secs(10));
    config.event_capacity = 1;
    let codex = Codex::connect(config).await.expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");
    let mut run = session.start("overflow").await.expect("run");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut observed_lag = false;
    while let Some(event) = run.next_event().await {
        match event.expect("event") {
            Event::Failed(error) => {
                assert_eq!(error.kind(), crate::ErrorKind::ConsumerLagged);
                observed_lag = true;
                break;
            }
            Event::Completed(result) => panic!("unexpected completion: {result:?}"),
            _ => {}
        }
    }
    assert!(observed_lag);

    let mut interrupt_observed = false;
    for _ in 0..20 {
        let diagnostics = codex.take_diagnostics().await;
        if diagnostics
            .iter()
            .any(|item| item.message == "interrupt observed")
        {
            interrupt_observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(interrupt_observed, "turn interrupt was not observed");
    session
        .close()
        .await
        .expect("close after terminal lag failure");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_failure_before_start_acknowledgement_does_not_leak_the_run_route() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), PRE_ACK_FAILURE_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");

    let error = session
        .start("malformed")
        .await
        .expect("run handle")
        .wait()
        .await
        .expect_err("malformed early completion must fail the run");
    assert_eq!(error.kind(), crate::ErrorKind::Protocol);
    session
        .close()
        .await
        .expect("terminal pre-ack failure must release the route");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_acknowledgement_event_overflow_interrupts_without_blocking_the_router() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), PRE_ACK_FAILURE_SCRIPT);
    let mut config = CodexConfig::new(package).with_request_timeout(Duration::from_secs(5));
    config.event_capacity = 1;
    let codex = Codex::connect(config).await.expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");

    let error = tokio::time::timeout(Duration::from_secs(2), async {
        session
            .start("overflow")
            .await
            .expect("run handle")
            .wait()
            .await
    })
    .await
    .expect("router must not wait for its own interrupt response")
    .expect_err("pre-ack queue overflow must fail the run");
    assert_eq!(error.kind(), crate::ErrorKind::ConsumerLagged);
    assert!(matches!(
        codex.account().await.expect("router remains responsive"),
        Account::SignedOut { .. }
    ));
    assert!(
        codex
            .take_diagnostics()
            .await
            .iter()
            .any(|item| item.message == "pre-ack overflow interrupt observed")
    );
    session
        .close()
        .await
        .expect("provider terminal must release the route");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_ack_terminal_overflow_uses_the_provider_terminal_without_interrupting_again() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), PRE_ACK_TERMINAL_OVERFLOW_SCRIPT);
    let mut config = CodexConfig::new(package).with_request_timeout(Duration::from_secs(5));
    config.event_capacity = 1;
    let codex = Codex::connect(config).await.expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");

    let run = tokio::time::timeout(Duration::from_secs(2), session.start("terminal overflow"))
        .await
        .expect("terminal-before-ack routing timeout")
        .expect("run handle");
    let error = run
        .wait()
        .await
        .expect_err("pre-ack terminal overflow must fail the run");
    assert_eq!(error.kind(), crate::ErrorKind::ConsumerLagged);

    assert!(matches!(
        codex.account().await.expect("router remains responsive"),
        Account::SignedOut { .. }
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        codex
            .take_diagnostics()
            .await
            .iter()
            .all(|item| item.message != "unexpected interrupt after provider terminal"),
        "a provider terminal notification must not trigger turn/interrupt"
    );

    session
        .close()
        .await
        .expect("provider terminal must release the route");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_in_flight_turn_start_ownership() {
    use std::sync::Arc;
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), START_SHUTDOWN_RACE_SCRIPT);
    let config = CodexConfig::new(package).with_request_timeout(Duration::from_secs(3));
    let codex = Codex::connect(config).await.expect("connect");
    let session = Arc::new(
        codex
            .session(SessionOptions::read_only(project_directory.path()).ephemeral())
            .await
            .expect("session"),
    );

    let start_task = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.start("start while shutdown begins").await }
    });

    let mut request_observed = false;
    for _ in 0..100 {
        let diagnostics = codex.take_diagnostics().await;
        if diagnostics
            .iter()
            .any(|item| item.message == "turn/start request observed before shutdown")
        {
            request_observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(request_observed, "turn/start request was not observed");

    tokio::time::timeout(Duration::from_secs(3), codex.shutdown())
        .await
        .expect("shutdown must not deadlock behind turn/start")
        .expect("shutdown must wait for turn/start ownership before unsubscribe");
    let run = tokio::time::timeout(Duration::from_secs(1), start_task)
        .await
        .expect("turn/start task timeout")
        .expect("turn/start task panicked")
        .expect("turn/start must return its owned run before shutdown cleanup");
    drop(run);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_one_shot_run_interrupts_and_unsubscribes_ephemeral_session() {
    use std::sync::Arc;
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let home_directory = tempfile::tempdir().expect("home tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let script = with_marker_root(CANCELLED_ONE_SHOT_SCRIPT, home_directory.path());
    let package = create_fake_package(package_directory.path(), &script);
    let codex = Arc::new(
        Codex::connect(CodexConfig::new(package))
            .await
            .expect("connect"),
    );
    let run = tokio::spawn({
        let codex = Arc::clone(&codex);
        let project = project_directory.path().to_path_buf();
        async move {
            codex
                .run("cancel this run", SessionOptions::read_only(project))
                .await
        }
    });

    let started = home_directory.path().join("one-shot-turn-started");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("one-shot turn must start");

    run.abort();
    let cancellation = run.await.expect_err("one-shot caller must be cancelled");
    assert!(cancellation.is_cancelled());

    let cleaned = home_directory.path().join("one-shot-cleaned");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !cleaned.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled one-shot session must be interrupted and unsubscribed");
    assert!(home_directory.path().join("one-shot-interrupted").exists());

    codex
        .account()
        .await
        .expect("successful cancellation cleanup keeps the runtime usable");
    let codex = Arc::try_unwrap(codex).ok().expect("sole Codex owner");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_shutdown_caller_does_not_cancel_owned_cleanup() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let home_directory = tempfile::tempdir().expect("home tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let script = with_marker_root(CANCELLED_SHUTDOWN_SCRIPT, home_directory.path());
    let package = create_fake_package(package_directory.path(), &script);
    let config = CodexConfig::new(package).with_shutdown_timeout(Duration::from_secs(2));
    let codex = Codex::connect(config).await.expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");

    let shutdown = tokio::spawn(async move { codex.shutdown().await });
    let unsubscribe_marker = home_directory.path().join("shutdown-unsubscribe-observed");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !unsubscribe_marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shutdown must reach remote cleanup");

    shutdown.abort();
    let cancellation = shutdown
        .await
        .expect_err("outer shutdown task must be cancelled");
    assert!(cancellation.is_cancelled());

    let stdin_closed = home_directory.path().join("shutdown-stdin-closed");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !stdin_closed.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned cleanup must continue after caller cancellation");

    drop(session);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_known_notification_fails_closed_at_the_connection_boundary() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), MALFORMED_NOTIFICATION_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");

    let error = session
        .start("malformed notification")
        .await
        .expect("run handle")
        .wait()
        .await
        .expect_err("known malformed notification must fail instead of being ignored");
    assert_eq!(error.kind(), crate::ErrorKind::Protocol);
    assert_eq!(error.operation(), "item.commandExecution.outputDelta");
    assert!(error.message().contains("delta"));

    let close_error = session
        .close()
        .await
        .expect_err("a pinned protocol violation makes the connection unusable");
    assert_eq!(close_error.kind(), crate::ErrorKind::Protocol);
    codex
        .shutdown()
        .await
        .expect("shutdown only needs to join local resources after forced disconnect");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_non_idempotent_create_response_disconnects_before_ownership_is_lost() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), MALFORMED_THREAD_START_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");

    let error = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect_err("a created thread without an id cannot remain untracked");
    assert_eq!(error.kind(), crate::ErrorKind::Protocol);
    assert_eq!(error.operation(), "thread.start");
    assert!(error.message().contains("thread.id"));

    let subsequent = codex
        .account()
        .await
        .expect_err("the connection must remain terminal after ownership was lost");
    assert_eq!(subsequent.kind(), crate::ErrorKind::Protocol);
    codex
        .shutdown()
        .await
        .expect("shutdown after forced disconnect");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_approval_sends_fail_closed_response() {
    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), APPROVAL_FAIL_CLOSED_SCRIPT);
    let codex = Codex::connect(CodexConfig::new(package))
        .await
        .expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");
    let mut run = session.start("approval").await.expect("run");
    loop {
        match run.next_event().await.expect("stream").expect("event") {
            Event::ApprovalRequested(request) => {
                drop(request);
                break;
            }
            other => assert!(matches!(other, Event::Started)),
        }
    }
    assert_eq!(
        run.wait().await.expect("completion").status,
        TurnStatus::Completed
    );
    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_deadline_sends_one_fail_closed_response() {
    use std::time::Duration;

    let package_directory = tempfile::tempdir().expect("package tempdir");
    let project_directory = tempfile::tempdir().expect("project tempdir");
    let package = create_fake_package(package_directory.path(), APPROVAL_FAIL_CLOSED_SCRIPT);
    let config = CodexConfig::new(package).with_approval_timeout(Duration::from_millis(80));
    let codex = Codex::connect(config).await.expect("connect");
    let session = codex
        .session(SessionOptions::read_only(project_directory.path()).ephemeral())
        .await
        .expect("session");
    let mut run = session.start("approval deadline").await.expect("run");
    let request = loop {
        match run.next_event().await.expect("stream").expect("event") {
            Event::ApprovalRequested(request) => break request,
            other => assert!(matches!(other, Event::Started)),
        }
    };
    tokio::time::sleep(Duration::from_millis(180)).await;
    drop(request);
    assert_eq!(
        run.wait().await.expect("completion").status,
        TurnStatus::Completed
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !codex
            .take_diagnostics()
            .await
            .iter()
            .any(|item| item.message == "duplicate approval response")
    );
    session.close().await.expect("close");
    codex.shutdown().await.expect("shutdown");
}
