//! Discovery and installation of the pinned Codex runtime.

use super::{PinnedRuntime, PinnedRuntimeDownload, RuntimeLock, RuntimePackage, format_digest};
use crate::error::{Error, ErrorKind, Result};
use flate2::read::GzDecoder;
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DOWNLOAD_TEMP_PREFIX: &str = ".vergerail-download-";
const INSTALL_TEMP_PREFIX: &str = ".vergerail-install-";

/// Controls whether resolution may download the pinned runtime when no usable copy exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadPolicy {
    /// Download, verify, and atomically install the pinned runtime when needed.
    IfMissing,
    /// Never access the network; return an error when no usable runtime exists.
    Never,
}

/// Describes where a resolved runtime came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeOrigin {
    /// The caller supplied an authoritative package root.
    Explicit,
    /// A byte-for-byte audited Codex installation was found on `PATH` or supplied explicitly.
    System,
    /// A previously installed Vergerail-managed runtime was reused.
    ManagedCache,
    /// Vergerail downloaded and installed the pinned runtime during this resolution.
    Downloaded,
}

/// A verified runtime selection plus its origin.
#[derive(Clone, Debug)]
pub struct ResolvedRuntime {
    package: RuntimePackage,
    origin: RuntimeOrigin,
}

impl ResolvedRuntime {
    /// Returns how this runtime was obtained.
    #[must_use]
    pub const fn origin(&self) -> RuntimeOrigin {
        self.origin
    }

    /// Returns the selected package.
    #[must_use]
    pub const fn package(&self) -> &RuntimePackage {
        &self.package
    }

    /// Consumes the resolution and returns the package for `CodexConfig`.
    #[must_use]
    pub fn into_package(self) -> RuntimePackage {
        self.package
    }
}

/// Resolves a canonical pinned Codex package or installs it in shared storage.
#[derive(Clone, Debug)]
pub struct RuntimeResolver {
    explicit_root: Option<PathBuf>,
    cache_root: Option<PathBuf>,
    download_policy: DownloadPolicy,
    search_system: bool,
    additional_system_candidates: Vec<PathBuf>,
}

impl Default for RuntimeResolver {
    fn default() -> Self {
        Self {
            explicit_root: None,
            cache_root: None,
            download_policy: DownloadPolicy::IfMissing,
            search_system: true,
            additional_system_candidates: Vec::new(),
        }
    }
}

impl RuntimeResolver {
    /// Creates a resolver that searches `PATH` for canonical audited packages and downloads if missing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses this package root authoritatively instead of discovery or download.
    #[must_use]
    pub fn with_explicit_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.explicit_root = Some(root.into());
        self
    }

    /// Overrides the Vergerail-managed shared runtime storage root.
    ///
    /// The final path must be a real, non-symlink directory. Vergerail secures it
    /// to owner-only access and maintains only Vergerail-prefixed temporary files.
    #[must_use]
    pub fn with_cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.cache_root = Some(root.into());
        self
    }

    /// Controls whether a missing runtime may be downloaded.
    #[must_use]
    pub const fn with_download_policy(mut self, policy: DownloadPolicy) -> Self {
        self.download_policy = policy;
        self
    }

    /// Enables or disables discovery of audited Codex installations on the host.
    #[must_use]
    pub const fn with_system_discovery(mut self, enabled: bool) -> Self {
        self.search_system = enabled;
        self
    }

    /// Adds a host-application-specific Codex executable discovery candidate.
    ///
    /// The executable is accepted only when it is the `bin/codex` entrypoint of
    /// the complete pinned package. This is useful when a desktop application
    /// knows an installation location that is not on `PATH`.
    #[must_use]
    pub fn with_system_candidate(mut self, entrypoint: impl Into<PathBuf>) -> Self {
        self.additional_system_candidates.push(entrypoint.into());
        self
    }

    /// Resolves and fully verifies a runtime, installing the pinned package when allowed.
    pub async fn resolve(self) -> Result<ResolvedRuntime> {
        let pinned = PinnedRuntime::load()?;
        let runtime_lock = pinned.lock().clone();

        if let Some(root) = self.explicit_root {
            let package = RuntimePackage::new(root, runtime_lock.clone());
            package.verify().await?;
            return Ok(ResolvedRuntime {
                package,
                origin: RuntimeOrigin::Explicit,
            });
        }

        if self.search_system {
            let mut candidates = self.additional_system_candidates;
            candidates.extend(system_candidates());
            let mut seen = HashSet::new();
            candidates.retain(|path| seen.insert(path.clone()));
            for candidate in candidates {
                if let Some(package) = package_for_system_candidate(&candidate, &runtime_lock)
                    && package.verify().await.is_ok()
                {
                    return Ok(ResolvedRuntime {
                        package,
                        origin: RuntimeOrigin::System,
                    });
                }
            }
        }

        let cache_root = match self.cache_root {
            Some(root) => root,
            None => default_cache_root()?,
        };
        match fs::symlink_metadata(&cache_root) {
            Ok(_) => secure_private_directory(&cache_root, "runtime.cache")?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(runtime_directory_error(
                    "runtime.cache",
                    format!("cannot inspect runtime cache root: {error}"),
                ));
            }
        }

        let managed_root = cache_root.join(managed_relative_root(&runtime_lock));
        match fs::symlink_metadata(&managed_root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                let package = RuntimePackage::new(&managed_root, runtime_lock.clone());
                match package.verify().await {
                    Ok(_) => {
                        return Ok(ResolvedRuntime {
                            package,
                            origin: RuntimeOrigin::ManagedCache,
                        });
                    }
                    Err(error) if error.operation() == "runtime.version" => return Err(error),
                    Err(_) => {}
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(runtime_directory_error(
                    "runtime.cache",
                    format!("cannot inspect managed runtime root: {error}"),
                ));
            }
        }

        if self.download_policy == DownloadPolicy::Never {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.resolve",
                "no audited system or managed Codex runtime is available and downloads are disabled",
            ));
        }

        let outcome = tokio::task::spawn_blocking(move || install_managed(&cache_root, &pinned))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.install",
                    format!("runtime installation worker failed: {error}"),
                )
            })??;
        let package = RuntimePackage::new(outcome.root, runtime_lock);
        if let Err(error) = package.verify().await {
            return Err(cleanup_failed_managed_verification(package.root(), error));
        }
        Ok(ResolvedRuntime {
            package,
            origin: if outcome.installed {
                RuntimeOrigin::Downloaded
            } else {
                RuntimeOrigin::ManagedCache
            },
        })
    }
}

fn cleanup_failed_managed_verification(root: &Path, error: Error) -> Error {
    if error.operation() == "runtime.version" {
        return error;
    }
    match remove_path(root) {
        Ok(()) => error,
        Err(cleanup_error) => error.with_related_error(
            "invalid managed runtime cleanup also failed",
            &cleanup_error,
        ),
    }
}

fn default_cache_root() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "runtime.cache",
            "HOME is not set; provide RuntimeResolver::with_cache_root",
        )
    })?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("vergerail")
        .join("runtimes"))
}

fn system_candidates() -> Vec<PathBuf> {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path)
                .map(|directory| directory.join("codex"))
                .filter(|path| path.is_file())
                .collect()
        })
        .unwrap_or_default()
}

fn package_for_system_candidate(
    candidate: &Path,
    runtime_lock: &RuntimeLock,
) -> Option<RuntimePackage> {
    let canonical = candidate.canonicalize().ok()?;
    for ancestor in canonical.ancestors().take(8) {
        let manifest = ancestor.join("codex-package.json");
        let packaged_entrypoint = ancestor.join("bin/codex");
        if manifest.is_file()
            && packaged_entrypoint
                .canonicalize()
                .is_ok_and(|path| path == canonical)
        {
            return Some(RuntimePackage::new(ancestor, runtime_lock.clone()));
        }
    }
    None
}

#[derive(Debug)]
struct InstallOutcome {
    root: PathBuf,
    installed: bool,
}

fn managed_relative_root(lock: &RuntimeLock) -> PathBuf {
    PathBuf::from("codex")
        .join(lock.version())
        .join(lock.target())
}

fn installation_lock_name(lock: &RuntimeLock) -> String {
    format!(".codex-{}-{}.lock", lock.version(), lock.target())
}

fn install_managed(cache_root: &Path, pinned: &PinnedRuntime) -> Result<InstallOutcome> {
    create_private_directory(cache_root)?;
    let runtime_lock = pinned.lock().clone();
    let final_root = cache_root.join(managed_relative_root(&runtime_lock));
    let lock_path = cache_root.join(installation_lock_name(&runtime_lock));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| install_error(format!("cannot open installation lock: {error}")))?;
    verify_open_lock_path(&lock_path, &lock)?;
    lock.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| install_error(format!("cannot secure installation lock: {error}")))?;
    fs2::FileExt::lock_exclusive(&lock)
        .map_err(|error| install_error(format!("cannot acquire installation lock: {error}")))?;
    verify_open_lock_path(&lock_path, &lock)?;
    cleanup_stale_installation_artifacts(cache_root)?;

    match fs::symlink_metadata(&final_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let cached = RuntimePackage::new(&final_root, runtime_lock.clone());
            if super::verify_filesystem(cached).is_ok() {
                return Ok(InstallOutcome {
                    root: final_root,
                    installed: false,
                });
            }
            remove_path(&final_root)?;
        }
        Ok(_) => remove_path(&final_root)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(install_error(format!(
                "cannot inspect managed runtime root: {error}"
            )));
        }
    }

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| install_error(format!("system clock is before Unix epoch: {error}")))?
        .as_nanos();
    let archive_path = cache_root.join(format!(
        "{DOWNLOAD_TEMP_PREFIX}{}-{unique}.tgz",
        std::process::id()
    ));
    let staging_root = cache_root.join(format!(
        "{INSTALL_TEMP_PREFIX}{}-{unique}",
        std::process::id()
    ));

    let result = (|| {
        download_archive(&archive_path, pinned.download())?;
        extract_runtime(&archive_path, &staging_root, pinned)?;
        if let Some(parent) = final_root.parent() {
            create_private_directory(parent)?;
        }
        fs::rename(&staging_root, &final_root)
            .map_err(|error| install_error(format!("cannot commit installed runtime: {error}")))?;
        Ok(InstallOutcome {
            root: final_root.clone(),
            installed: true,
        })
    })();
    let cleanup = cleanup_installation_attempt(&archive_path, &staging_root);
    match (result, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error.with_related_error(
            "temporary installation artifact cleanup also failed",
            &cleanup_error,
        )),
    }
}

fn cleanup_installation_attempt(archive_path: &Path, staging_root: &Path) -> Result<()> {
    let mut failure: Option<Error> = None;
    for path in [archive_path, staging_root] {
        let cleanup = match fs::symlink_metadata(path) {
            Ok(_) => remove_path(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(install_error(format!(
                "cannot inspect temporary runtime artifact {}: {error}",
                path.display()
            ))),
        };
        if let Err(error) = cleanup {
            failure = Some(match failure {
                None => error,
                Some(primary) => primary.with_related_error(
                    "another temporary runtime artifact cleanup failed",
                    &error,
                ),
            });
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn download_archive(destination: &Path, download: &PinnedRuntimeDownload) -> Result<()> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(DOWNLOAD_TOTAL_TIMEOUT))
        .timeout_connect(Some(DOWNLOAD_CONNECT_TIMEOUT))
        .timeout_recv_body(Some(DOWNLOAD_READ_TIMEOUT))
        .build()
        .new_agent();
    let response = agent
        .get(download.url())
        .call()
        .map_err(|error| install_error(format!("official runtime download failed: {error}")))?;
    if response.status() != 200 {
        return Err(install_error(format!(
            "official runtime download returned HTTP {}",
            response.status()
        )));
    }
    let mut source = response
        .into_body()
        .into_reader()
        .take(download.bytes() + 1);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| install_error(format!("cannot create runtime archive: {error}")))?;
    output
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| install_error(format!("cannot secure runtime archive: {error}")))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let started = Instant::now();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| install_error(format!("cannot read runtime download: {error}")))?;
        if count == 0 {
            break;
        }
        if started.elapsed() > DOWNLOAD_TOTAL_TIMEOUT {
            return Err(install_error("runtime download exceeded 15 minutes"));
        }
        total += count as u64;
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| install_error(format!("cannot write runtime archive: {error}")))?;
    }
    output
        .sync_all()
        .map_err(|error| install_error(format!("cannot sync runtime archive: {error}")))?;
    let observed = format_digest(hasher.finalize());
    if total != download.bytes() || observed != download.sha256() {
        return Err(install_error(format!(
            "runtime archive identity mismatch: expected {} bytes and {}, observed {total} bytes and {observed}",
            download.bytes(),
            download.sha256()
        )));
    }
    Ok(())
}

fn verify_open_lock_path(path: &Path, file: &File) -> Result<()> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| install_error(format!("cannot inspect installation lock: {error}")))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(install_error(format!(
            "{} must be a regular non-symlink installation lock",
            path.display()
        )));
    }
    let file_metadata = file.metadata().map_err(|error| {
        install_error(format!("cannot inspect open installation lock: {error}"))
    })?;
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(install_error(
            "installation lock path changed while it was being opened",
        ));
    }
    Ok(())
}

fn cleanup_stale_installation_artifacts(cache_root: &Path) -> Result<()> {
    for entry in fs::read_dir(cache_root)
        .map_err(|error| install_error(format!("cannot inspect runtime cache: {error}")))?
    {
        let entry = entry.map_err(|error| {
            install_error(format!("cannot inspect runtime cache entry: {error}"))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(DOWNLOAD_TEMP_PREFIX) || name.starts_with(INSTALL_TEMP_PREFIX) {
            remove_path(&entry.path())?;
        }
    }
    Ok(())
}

fn extract_runtime(archive_path: &Path, staging_root: &Path, pinned: &PinnedRuntime) -> Result<()> {
    create_private_directory(staging_root)?;
    let lock = pinned.lock();
    let expected = lock
        .artifacts
        .iter()
        .map(|artifact| (artifact.relative_path.clone(), artifact.executable))
        .collect::<HashMap<_, _>>();
    let archive = File::open(archive_path)
        .map_err(|error| install_error(format!("cannot open runtime archive: {error}")))?;
    let mut archive = tar::Archive::new(GzDecoder::new(archive));
    let mut extracted = HashSet::new();
    let entries = archive
        .entries()
        .map_err(|error| install_error(format!("cannot read runtime archive: {error}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            install_error(format!("cannot read runtime archive entry: {error}"))
        })?;
        let path = entry
            .path()
            .map_err(|error| install_error(format!("runtime archive path is invalid: {error}")))?;
        let Ok(relative) = path
            .strip_prefix(pinned.download().archive_prefix())
            .map(Path::to_owned)
        else {
            continue;
        };
        let Some(executable) = expected.get(&relative).copied() else {
            continue;
        };
        if !entry.header().entry_type().is_file() || !extracted.insert(relative.clone()) {
            return Err(install_error(format!(
                "runtime archive contains invalid or duplicate artifact {}",
                relative.display()
            )));
        }
        let destination = staging_root.join(&relative);
        if let Some(parent) = destination.parent() {
            create_private_directory(parent)?;
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(|error| install_error(format!("cannot create runtime artifact: {error}")))?;
        io::copy(&mut entry, &mut output)
            .map_err(|error| install_error(format!("cannot extract runtime artifact: {error}")))?;
        output
            .sync_all()
            .map_err(|error| install_error(format!("cannot sync runtime artifact: {error}")))?;
        output
            .set_permissions(fs::Permissions::from_mode(if executable {
                0o755
            } else {
                0o644
            }))
            .map_err(|error| install_error(format!("cannot set runtime permissions: {error}")))?;
    }
    if extracted.len() != expected.len() {
        return Err(install_error(
            "runtime archive is missing required artifacts",
        ));
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| {
                install_error(format!("cannot create runtime directory: {error}"))
            })?;
        }
        Err(error) => {
            return Err(install_error(format!(
                "cannot inspect runtime directory: {error}"
            )));
        }
    }
    secure_private_directory(path, "runtime.install")
}

fn secure_private_directory(path: &Path, operation: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        runtime_directory_error(
            operation,
            format!("cannot inspect runtime directory: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(runtime_directory_error(
            operation,
            format!(
                "{} must be a real, non-symlink runtime directory",
                path.display()
            ),
        ));
    }

    let directory = File::open(path).map_err(|error| {
        runtime_directory_error(operation, format!("cannot open runtime directory: {error}"))
    })?;
    verify_open_directory_path(path, &directory, operation)?;
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| {
            runtime_directory_error(
                operation,
                format!("cannot secure runtime directory: {error}"),
            )
        })?;
    verify_open_directory_path(path, &directory, operation)
}

fn verify_open_directory_path(
    path: &Path,
    directory: &File,
    operation: &'static str,
) -> Result<()> {
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        runtime_directory_error(
            operation,
            format!("cannot inspect runtime directory: {error}"),
        )
    })?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_dir() {
        return Err(runtime_directory_error(
            operation,
            format!(
                "{} must remain a real, non-symlink runtime directory",
                path.display()
            ),
        ));
    }
    let file_metadata = directory.metadata().map_err(|error| {
        runtime_directory_error(
            operation,
            format!("cannot inspect open runtime directory: {error}"),
        )
    })?;
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(runtime_directory_error(
            operation,
            "runtime directory path changed while it was being secured",
        ));
    }
    Ok(())
}

fn runtime_directory_error(operation: &'static str, message: impl Into<String>) -> Error {
    Error::new(ErrorKind::RuntimeVerification, operation, message)
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| install_error(format!("cannot inspect stale runtime path: {error}")))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| install_error(format!("cannot remove stale runtime path: {error}")))
}

fn install_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::RuntimeVerification, "runtime.install", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    #[tokio::test]
    async fn never_policy_reports_missing_runtime_without_network() {
        let cache = tempfile::tempdir().expect("cache");
        let error = RuntimeResolver::new()
            .with_system_discovery(false)
            .with_cache_root(cache.path())
            .with_download_policy(DownloadPolicy::Never)
            .resolve()
            .await
            .expect_err("missing runtime must fail");
        assert_eq!(error.operation(), "runtime.resolve");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolver_rejects_a_symlink_cache_root_without_changing_its_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("target permissions");
        let link = directory.path().join("cache-link");
        symlink(&target, &link).expect("cache symlink");

        let error = RuntimeResolver::new()
            .with_system_discovery(false)
            .with_cache_root(&link)
            .with_download_policy(DownloadPolicy::Never)
            .resolve()
            .await
            .expect_err("cache root symlink must be rejected");

        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
        assert_eq!(error.operation(), "runtime.cache");
        assert_eq!(
            fs::metadata(&target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn loose_system_binary_is_not_a_canonical_package() {
        let directory = tempfile::tempdir().expect("tempdir");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let candidate = bin.join("codex");
        fs::write(&candidate, b"not a package").expect("candidate");

        let pinned = PinnedRuntime::load().expect("pinned runtime");
        assert!(package_for_system_candidate(&candidate, pinned.lock()).is_none());
    }

    #[test]
    fn launch_failure_preserves_statically_verified_managed_runtime() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().join("runtime");
        fs::create_dir(&root).expect("runtime root");
        fs::write(root.join("keep"), b"verified bytes").expect("runtime file");

        let error = Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            "version command timed out",
        );
        let returned = cleanup_failed_managed_verification(&root, error);

        assert_eq!(returned.operation(), "runtime.version");
        assert!(root.join("keep").is_file());
    }

    #[test]
    fn static_verification_failure_removes_invalid_managed_runtime() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().join("runtime");
        fs::create_dir(&root).expect("runtime root");
        fs::write(root.join("invalid"), b"wrong bytes").expect("runtime file");

        let error = Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.hash",
            "artifact hash mismatch",
        );
        let returned = cleanup_failed_managed_verification(&root, error);

        assert_eq!(returned.operation(), "runtime.hash");
        assert!(!root.exists());
    }

    #[test]
    fn extraction_materializes_only_the_audited_runtime_file_set() {
        let directory = tempfile::tempdir().expect("tempdir");
        let archive_path = directory.path().join("runtime.tgz");
        let archive = File::create(&archive_path).expect("archive");
        let encoder = GzEncoder::new(archive, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let pinned = PinnedRuntime::load().expect("pinned runtime");
        let lock = pinned.lock();
        for artifact in &lock.artifacts {
            let contents = artifact.relative_path.to_string_lossy().into_owned();
            append_file(
                &mut builder,
                &format!(
                    "{}/{}",
                    pinned.download().archive_prefix().display(),
                    artifact.relative_path.display()
                ),
                contents.as_bytes(),
            );
        }
        append_file(
            &mut builder,
            "package/README.md",
            b"not part of the runtime",
        );
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");

        let destination = directory.path().join("installed");
        extract_runtime(&archive_path, &destination, &pinned).expect("extract");
        for artifact in &lock.artifacts {
            assert!(destination.join(&artifact.relative_path).is_file());
        }
        assert!(!destination.join("package/README.md").exists());
    }

    #[test]
    fn stale_installation_artifacts_are_removed_without_touching_other_files() {
        let cache = tempfile::tempdir().expect("cache");
        let stale_archive = cache
            .path()
            .join(format!("{DOWNLOAD_TEMP_PREFIX}stale.tgz"));
        let stale_directory = cache.path().join(format!("{INSTALL_TEMP_PREFIX}stale"));
        fs::write(&stale_archive, b"partial").expect("stale file");
        fs::create_dir(&stale_directory).expect("stale directory");
        fs::write(cache.path().join(".download-foreign.tgz"), b"foreign")
            .expect("foreign download");
        fs::create_dir(cache.path().join(".install-foreign")).expect("foreign install");
        fs::write(cache.path().join("keep"), b"keep").expect("kept file");

        cleanup_stale_installation_artifacts(cache.path()).expect("cleanup");

        assert!(!stale_archive.exists());
        assert!(!stale_directory.exists());
        assert!(cache.path().join(".download-foreign.tgz").is_file());
        assert!(cache.path().join(".install-foreign").is_dir());
        assert!(cache.path().join("keep").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn installation_lock_rejects_a_symlink_without_changing_its_target() {
        use std::os::unix::fs::symlink;

        let cache = tempfile::tempdir().expect("cache");
        let target = cache.path().join("lock-target");
        fs::write(&target, b"foreign").expect("lock target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
            .expect("target permissions");
        let pinned = PinnedRuntime::load().expect("pinned runtime");
        let lock_path = cache.path().join(installation_lock_name(pinned.lock()));
        symlink(&target, &lock_path).expect("lock symlink");

        let error =
            install_managed(cache.path(), &pinned).expect_err("lock symlink must be rejected");

        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
        assert_eq!(error.operation(), "runtime.install");
        assert_eq!(fs::read(&target).expect("target remains"), b"foreign");
        assert_eq!(
            fs::metadata(&target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn cache_identity_is_derived_from_the_runtime_lock() {
        let pinned = PinnedRuntime::load().expect("pinned runtime");
        assert_eq!(
            managed_relative_root(pinned.lock()),
            PathBuf::from("codex/0.149.1/aarch64-apple-darwin")
        );
        assert_eq!(
            installation_lock_name(pinned.lock()),
            ".codex-0.149.1-aarch64-apple-darwin.lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_runtime_directory_rejects_a_symlink_without_changing_its_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("target permissions");
        let link = directory.path().join("cache-link");
        symlink(&target, &link).expect("cache symlink");

        let error = create_private_directory(&link).expect_err("symlink must be rejected");

        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
        assert_eq!(error.operation(), "runtime.install");
        assert_eq!(
            fs::metadata(&target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    fn append_file(builder: &mut tar::Builder<GzEncoder<File>>, path: &str, contents: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, contents)
            .expect("append file");
    }
}
