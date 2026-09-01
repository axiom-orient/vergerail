//! Smoke test against the audited official Codex package.

use vergerail::{
    Account, Codex, CodexConfig, DownloadPolicy, RuntimeOrigin, RuntimePackage, RuntimeResolver,
};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use serde_json::Value;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::path::PathBuf;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::process::Stdio;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::time::{Duration, Instant};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use tokio::io::AsyncWriteExt as _;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use tokio::time::timeout;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn provider_binary() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_vergerail-upagent-provider")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_vergerail_upagent_provider"))
        .map(PathBuf::from)
        .expect("Cargo must expose the vergerail-upagent-provider test binary")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn installed_runtime_root() -> Option<PathBuf> {
    std::env::var_os("VERGERAIL_CODEX_PACKAGE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home).join(
                    "Library/Application Support/vergerail/runtimes/codex/0.150.1/aarch64-apple-darwin",
                )
            })
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VERGERAIL_CODEX_PACKAGE pointing at the official 0.150.1 macOS package"]
async fn connects_to_pinned_official_runtime_and_reuses_standard_account() {
    let package_root = std::env::var_os("VERGERAIL_CODEX_PACKAGE")
        .expect("VERGERAIL_CODEX_PACKAGE must be set for this ignored test");
    let runtime = RuntimePackage::pinned(package_root).expect("audited runtime selection");
    let codex = Codex::connect(CodexConfig::new(runtime))
        .await
        .expect("connect to official app-server");
    assert!(matches!(
        codex.account().await.expect("account/read"),
        Account::ChatGpt { .. }
    ));
    codex.shutdown().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VERGERAIL_CODEX_PACKAGE pointing at the official 0.150.1 macOS package"]
async fn resolver_reuses_an_audited_system_install_without_downloading() {
    let package_root = std::path::PathBuf::from(
        std::env::var_os("VERGERAIL_CODEX_PACKAGE")
            .expect("VERGERAIL_CODEX_PACKAGE must be set for this ignored test"),
    );
    let expected_root = package_root
        .canonicalize()
        .expect("official package root must canonicalize");
    let cache = tempfile::tempdir().expect("empty managed cache");
    let resolved = RuntimeResolver::new()
        .with_system_discovery(true)
        .with_system_candidate(package_root.join("bin/codex"))
        .with_cache_root(cache.path())
        .with_download_policy(DownloadPolicy::Never)
        .resolve()
        .await
        .expect("reuse audited system package");
    assert_eq!(resolved.origin(), RuntimeOrigin::System);
    assert_eq!(resolved.package().root(), expected_root);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_runtime_verification_deadline_reaps_the_guardian_helper() {
    let Some(package_root) = installed_runtime_root() else {
        eprintln!("skipping runtime helper proof: HOME is not set");
        return;
    };
    if !package_root.is_dir() {
        eprintln!(
            "skipping runtime helper proof: package is not installed at {}",
            package_root.display()
        );
        return;
    }
    let temporary_root = std::env::temp_dir();
    let existing_helpers = std::fs::read_dir(&temporary_root)
        .expect("temporary directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".vergerail-runtime-verify-")
        })
        .count();
    assert_eq!(existing_helpers, 0, "no prior verifier helper may survive");

    let request = serde_json::json!({
        "schemaVersion": 2,
        "requestId": "runtime-helper-timeout",
        "operation": "model_turn",
        "messages": [{
            "role": "user",
            "content": "unused",
            "contentParts": [],
            "toolCalls": [],
            "toolCallId": null,
            "toolName": null,
            "isError": false
        }],
        "observations": [],
        "tools": [],
        "reasoning": "off",
        "timeoutMs": 100,
        "maximumResponseBytes": 1024,
        "prompt": null,
        "imageOptions": null
    });
    let started = Instant::now();
    let mut child = tokio::process::Command::new(provider_binary())
        .env("VERGERAIL_CODEX_PACKAGE", &package_root)
        .env("VERGERAIL_MODEL", "gpt-5.6-luna")
        .env("VERGERAIL_WORKSPACE", "/tmp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("provider process must start");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(
            serde_json::to_string(&request)
                .expect("request JSON")
                .as_bytes(),
        )
        .await
        .expect("request input");
    let output = timeout(Duration::from_secs(8), child.wait_with_output())
        .await
        .expect("provider deadline must be bounded")
        .expect("provider process must exit");
    assert!(
        output.status.success(),
        "typed timeout must be a process success"
    );
    assert!(
        output.stderr.is_empty(),
        "provider protocol must stay on stdout"
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("strict response");
    assert_eq!(response["error"]["code"], "timeout");
    assert!(started.elapsed() < Duration::from_secs(8));

    let remaining_helpers = std::fs::read_dir(&temporary_root)
        .expect("temporary directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".vergerail-runtime-verify-")
        })
        .count();
    assert_eq!(
        remaining_helpers, 0,
        "guardian helper artifacts must be removed"
    );
}
