//! Live installation test for Vergerail-managed Codex runtime provisioning.

use vergerail::{Account, Codex, CodexConfig, DownloadPolicy, RuntimeOrigin, RuntimeResolver};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads and installs the pinned 0.149.1 macOS runtime"]
async fn downloads_connects_and_reuses_the_managed_runtime() {
    let cache = tempfile::tempdir().expect("isolated shared runtime cache");
    let home = tempfile::tempdir().expect("isolated CODEX_HOME");
    let resolved = RuntimeResolver::new()
        .with_system_discovery(false)
        .with_cache_root(cache.path())
        .resolve()
        .await
        .expect("download and verify managed runtime");
    assert_eq!(resolved.origin(), RuntimeOrigin::Downloaded);

    let codex = Codex::connect(CodexConfig::new(resolved.into_package(), home.path()))
        .await
        .expect("connect to managed app-server");
    assert!(matches!(
        codex.account().await.expect("account/read"),
        Account::SignedOut { .. }
    ));
    codex.shutdown().await.expect("clean shutdown");

    let reused = RuntimeResolver::new()
        .with_system_discovery(false)
        .with_cache_root(cache.path())
        .with_download_policy(DownloadPolicy::Never)
        .resolve()
        .await
        .expect("reuse managed runtime without network");
    assert_eq!(reused.origin(), RuntimeOrigin::ManagedCache);
}
