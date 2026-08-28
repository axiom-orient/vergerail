//! Smoke test against the audited official Codex package.

use vergerail::{
    Account, Codex, CodexConfig, DownloadPolicy, RuntimeOrigin, RuntimePackage, RuntimeResolver,
};

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
