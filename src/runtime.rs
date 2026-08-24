//! Verification of a pinned canonical Codex package.

use crate::error::{Error, ErrorKind, Result};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::private::process_tree;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::process::Stdio;
use std::time::Duration;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use tokio::io::{AsyncRead, AsyncReadExt as _};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use tokio::time::timeout;

mod lock;
mod manager;

pub(crate) use lock::{PinnedRuntime, PinnedRuntimeDownload, RuntimeArtifact, RuntimeLock};
pub use manager::{DownloadPolicy, ResolvedRuntime, RuntimeOrigin, RuntimeResolver};

const PINNED_SCHEMA: &[u8] =
    include_bytes!("../protocol/codex-0.149.1/codex_app_server_protocol.v2.schemas.json");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const VERSION_OUTPUT_LIMIT: usize = 8 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const VERSION_CLEANUP_GRACE: Duration = Duration::from_secs(1);

/// A canonical Codex package root plus its required identity.
#[derive(Clone, Debug)]
pub struct RuntimePackage {
    root: PathBuf,
    lock: RuntimeLock,
}

impl RuntimePackage {
    /// Selects the repository's audited pinned Codex package.
    ///
    /// The package directory is verified byte-for-byte before execution. Runtime
    /// identity and download metadata come from the embedded runtime lock.
    pub fn pinned(root: impl Into<PathBuf>) -> Result<Self> {
        let pinned = PinnedRuntime::load()?;
        Ok(Self::new(root, pinned.lock().clone()))
    }

    pub(crate) fn new(root: impl Into<PathBuf>, lock: RuntimeLock) -> Self {
        Self {
            root: root.into(),
            lock,
        }
    }

    /// Returns the configured package root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the exact Codex version required by this package selection.
    #[must_use]
    pub fn version(&self) -> &str {
        self.lock.version()
    }

    /// Returns the exact upstream Codex commit associated with this package.
    #[must_use]
    pub fn upstream_commit(&self) -> &str {
        self.lock.upstream_commit()
    }

    /// Returns the required runtime target triple.
    #[must_use]
    pub fn target(&self) -> &str {
        self.lock.target()
    }

    /// Returns the canonical JSON SHA-256 of the generated v2 protocol schema.
    #[must_use]
    pub fn protocol_schema_canonical_sha256(&self) -> &str {
        self.lock.protocol_schema_canonical_sha256()
    }

    pub(crate) async fn verify(&self) -> Result<VerifiedRuntime> {
        verify_protocol_schema(&self.lock)?;
        verify_host_target(&self.lock.target)?;

        let package = self.clone();
        let verified = tokio::task::spawn_blocking(move || verify_filesystem(package))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.verify",
                    format!("verification worker failed: {error}"),
                )
            })??;

        verify_version(&verified.entrypoint, &self.lock.version).await?;
        Ok(verified)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedRuntime {
    pub(crate) root: PathBuf,
    pub(crate) entrypoint: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackageManifest {
    layout_version: u64,
    version: String,
    target: String,
    variant: String,
    entrypoint: String,
    resources_dir: String,
    path_dir: String,
}

fn verify_protocol_schema(lock: &RuntimeLock) -> Result<()> {
    #[cfg(test)]
    if lock.version == "0.test" {
        return Ok(());
    }
    let observed = canonical_json_sha256(PINNED_SCHEMA)?;
    if observed != lock.protocol_schema_canonical_sha256 {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.schema",
            format!(
                "embedded protocol schema canonical SHA-256 mismatch: expected {}, observed {observed}",
                lock.protocol_schema_canonical_sha256
            ),
        ));
    }
    Ok(())
}

fn verify_host_target(target: &str) -> Result<()> {
    #[cfg(test)]
    if target == "test-target" {
        return Ok(());
    }

    let compatible = matches!(
        (std::env::consts::OS, std::env::consts::ARCH, target),
        ("macos", "aarch64", "aarch64-apple-darwin")
    );
    if compatible {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.target",
            format!(
                "runtime target '{target}' is not compatible with host '{}-{}'",
                std::env::consts::ARCH,
                std::env::consts::OS
            ),
        ))
    }
}

fn verify_filesystem(package: RuntimePackage) -> Result<VerifiedRuntime> {
    let root_metadata = fs::symlink_metadata(&package.root).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.canonicalize",
            format!("cannot inspect package root: {error}"),
        )
    })?;
    if root_metadata.file_type().is_symlink() {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.layout",
            "package root may not be a symbolic link",
        ));
    }
    let root = package.root.canonicalize().map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.canonicalize",
            format!("cannot canonicalize package root: {error}"),
        )
    })?;
    if !root.is_dir() {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.layout",
            "package root is not a directory",
        ));
    }
    verify_secure_permissions(&root, false)?;
    verify_exact_file_set(&root, &package.lock.artifacts)?;

    for artifact in &package.lock.artifacts {
        let joined = root.join(&artifact.relative_path);
        let metadata = fs::symlink_metadata(&joined).map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.artifact",
                format!("missing {}: {error}", artifact.relative_path.display()),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.artifact",
                format!(
                    "{} must be a regular non-symlink file",
                    artifact.relative_path.display()
                ),
            ));
        }
        let canonical = joined.canonicalize().map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.artifact",
                format!(
                    "cannot resolve {}: {error}",
                    artifact.relative_path.display()
                ),
            )
        })?;
        if !canonical.starts_with(&root) {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.artifact",
                format!(
                    "{} resolves outside the package",
                    artifact.relative_path.display()
                ),
            ));
        }
        verify_secure_permissions(&canonical, artifact.executable)?;
        let actual = hash_file(&canonical)?;
        if actual != artifact.sha256 {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.hash",
                format!(
                    "SHA-256 mismatch for {}: expected {}, observed {actual}",
                    artifact.relative_path.display(),
                    artifact.sha256
                ),
            ));
        }
    }

    let manifest_path = root.join("codex-package.json");
    let manifest: PackageManifest =
        serde_json::from_reader(File::open(&manifest_path).map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.manifest",
                error.to_string(),
            )
        })?)
        .map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.manifest",
                format!("invalid codex-package.json: {error}"),
            )
        })?;
    if manifest.layout_version != 1
        || manifest.version != package.lock.version
        || manifest.target != package.lock.target
        || manifest.variant != package.lock.variant
        || Path::new(&manifest.entrypoint) != package.lock.entrypoint
        || manifest.resources_dir != "codex-resources"
        || manifest.path_dir != "codex-path"
    {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.manifest",
            "package manifest does not match the runtime lock",
        ));
    }
    for directory in [&manifest.resources_dir, &manifest.path_dir] {
        if !root.join(directory).is_dir() {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.manifest",
                format!("required package directory '{directory}' is missing"),
            ));
        }
    }

    Ok(VerifiedRuntime {
        entrypoint: root.join(&package.lock.entrypoint),
        root,
    })
}

fn verify_exact_file_set(root: &Path, artifacts: &[RuntimeArtifact]) -> Result<()> {
    let expected_files = artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<HashSet<_>>();
    let mut expected_directories = HashSet::from([
        PathBuf::from("bin"),
        PathBuf::from("codex-path"),
        PathBuf::from("codex-resources"),
    ]);
    for artifact in artifacts {
        let mut current = artifact.relative_path.parent();
        while let Some(directory) = current {
            if directory.as_os_str().is_empty() {
                break;
            }
            expected_directories.insert(directory.to_path_buf());
            current = directory.parent();
        }
    }

    let mut observed_files = HashSet::new();
    let mut observed_directories = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.layout",
                format!("cannot read {}: {error}", directory.display()),
            )
        })? {
            let entry = entry.map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.layout",
                    error.to_string(),
                )
            })?;
            let file_type = entry.file_type().map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.layout",
                    error.to_string(),
                )
            })?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.layout",
                    error.to_string(),
                )
            })?;
            if file_type.is_symlink() {
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.layout",
                    format!("symbolic links are not accepted: {}", relative.display()),
                ));
            }
            if file_type.is_dir() {
                if !expected_directories.contains(relative) {
                    return Err(Error::new(
                        ErrorKind::RuntimeVerification,
                        "runtime.layout",
                        format!("unexpected runtime directory: {}", relative.display()),
                    ));
                }
                verify_secure_permissions(&path, false)?;
                observed_directories.insert(relative.to_path_buf());
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.layout",
                    format!("unsupported filesystem entry: {}", relative.display()),
                ));
            }
            if !expected_files.contains(relative) {
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.layout",
                    format!("unexpected runtime file: {}", relative.display()),
                ));
            }
            observed_files.insert(relative.to_path_buf());
        }
    }
    if observed_files != expected_files {
        let missing = expected_files
            .difference(&observed_files)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.layout",
            format!("runtime package is missing locked files: {missing}"),
        ));
    }
    if observed_directories != expected_directories {
        let missing = expected_directories
            .difference(&observed_directories)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.layout",
            format!("runtime package is missing locked directories: {missing}"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_secure_permissions(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::symlink_metadata(path)
        .map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.permissions",
                error.to_string(),
            )
        })?
        .permissions()
        .mode();
    if mode & 0o022 != 0 {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.permissions",
            format!("{} is writable by group or others", path.display()),
        ));
    }
    if executable && mode & 0o111 == 0 {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.permissions",
            format!("{} is not executable", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_secure_permissions(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

async fn verify_version(entrypoint: &Path, version: &str) -> Result<()> {
    verify_version_with_timeout(entrypoint, version, VERSION_TIMEOUT).await
}

async fn verify_version_with_timeout(
    entrypoint: &Path,
    version: &str,
    version_timeout: Duration,
) -> Result<()> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (entrypoint, version, version_timeout);
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            "the guardian runtime is supported only on aarch64 macOS",
        ));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        verify_version_with_timeout_supported(entrypoint, version, version_timeout).await
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
async fn verify_version_with_timeout_supported(
    entrypoint: &Path,
    version: &str,
    version_timeout: Duration,
) -> Result<()> {
    let guardian_directory =
        process_tree::create_guardian_directory(&std::env::temp_dir(), "vergerail-version")
            .map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.guardian",
                    format!("cannot create a private guardian directory: {error}"),
                )
            })?;
    let guardian_path = match process_tree::extract_guardian(&guardian_directory) {
        Ok(path) => path,
        Err(error) => {
            let _ = process_tree::remove_guardian_directory(&guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.guardian",
                format!("cannot materialize the audited guardian: {error}"),
            ));
        }
    };

    let mut command = process_tree::command(&guardian_path, entrypoint);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = process_tree::remove_guardian(&guardian_path);
            let _ = process_tree::remove_guardian_directory(&guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("failed to execute pinned entrypoint guardian: {error}"),
            ));
        }
    };
    let identity = match process_tree::capture(&child) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = process_tree::remove_guardian(&guardian_path);
            let _ = process_tree::remove_guardian_directory(&guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("cannot capture guardian process identity: {error}"),
            ));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = cleanup_version_artifacts(&guardian_path, &guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                "version stdout was not piped",
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = cleanup_version_artifacts(&guardian_path, &guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                "version stderr was not piped",
            ));
        }
    };
    let mut stdout_task = tokio::spawn(read_bounded_output(stdout, VERSION_OUTPUT_LIMIT));
    let mut stderr_task = tokio::spawn(read_bounded_output(stderr, VERSION_OUTPUT_LIMIT));

    let status = match timeout(version_timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            terminate_version_process(
                identity,
                &mut child,
                &mut stdout_task,
                &mut stderr_task,
                &guardian_path,
                &guardian_directory,
            )
            .await;
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("failed waiting for pinned entrypoint: {error}"),
            ));
        }
        Err(_) => {
            let cleanup = terminate_version_process(
                identity,
                &mut child,
                &mut stdout_task,
                &mut stderr_task,
                &guardian_path,
                &guardian_directory,
            )
            .await;
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!(
                    "version command exceeded {} ms and was terminated{cleanup}",
                    version_timeout.as_millis()
                ),
            ));
        }
    };

    let outputs = timeout(VERSION_CLEANUP_GRACE, async {
        let stdout = (&mut stdout_task).await.map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("version stdout reader failed: {error}"),
            )
        })??;
        let stderr = (&mut stderr_task).await.map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("version stderr reader failed: {error}"),
            )
        })??;
        Ok::<_, Error>((stdout, stderr))
    })
    .await;
    let (stdout, stderr) = match outputs {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("{error}{cleanup}"),
            ));
        }
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("version output pipes remained open after the entrypoint exited{cleanup}"),
            ));
        }
    };

    if stdout.overflowed || stderr.overflowed {
        let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            format!("version output exceeded {VERSION_OUTPUT_LIMIT} bytes{cleanup}"),
        ));
    }
    if !status.success() {
        let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            format!("entrypoint exited with {status}{cleanup}"),
        ));
    }
    let stdout = match String::from_utf8(stdout.bytes) {
        Ok(stdout) => stdout,
        Err(_) => {
            let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("version output was not UTF-8{cleanup}"),
            ));
        }
    };
    let observed = stdout.trim();
    let expected = format!("codex-cli {version}");
    if observed != expected {
        let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            format!("expected '{expected}', observed '{observed}'{cleanup}"),
        ));
    }
    let cleanup = cleanup_version_artifacts(&guardian_path, &guardian_directory);
    if !cleanup.is_empty() {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.guardian",
            cleanup.trim_start_matches(';').trim().to_owned(),
        ));
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
async fn terminate_version_process(
    identity: process_tree::ProcessIdentity,
    child: &mut tokio::process::Child,
    stdout_task: &mut tokio::task::JoinHandle<Result<BoundedOutput>>,
    stderr_task: &mut tokio::task::JoinHandle<Result<BoundedOutput>>,
    guardian_path: &Path,
    guardian_directory: &Path,
) -> String {
    let kill_error = process_tree::terminate(identity, child).err();
    stdout_task.abort();
    stderr_task.abort();

    let cleanup_timed_out = timeout(VERSION_CLEANUP_GRACE, async {
        let wait_result = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        wait_result
    })
    .await;

    let mut result = match (kill_error, cleanup_timed_out) {
        (None, Ok(Ok(_))) => String::new(),
        (Some(error), Ok(Ok(_))) => format!("; guardian termination failed: {error}"),
        (None, Ok(Err(error))) => format!("; guardian reap failed: {error}"),
        (Some(kill), Ok(Err(reap))) => {
            format!("; guardian termination failed: {kill}; guardian reap failed: {reap}")
        }
        (None, Err(_)) => "; guardian cleanup exceeded 1000 ms".to_owned(),
        (Some(error), Err(_)) => {
            format!("; guardian termination failed: {error}; cleanup exceeded 1000 ms")
        }
    };
    result.push_str(&cleanup_version_artifacts(
        guardian_path,
        guardian_directory,
    ));
    result
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn cleanup_version_artifacts(helper: &Path, directory: &Path) -> String {
    let mut failures = Vec::new();
    if let Err(error) = process_tree::remove_guardian(helper) {
        failures.push(format!("guardian helper removal failed: {error}"));
    }
    if let Err(error) = process_tree::remove_guardian_directory(directory) {
        failures.push(format!("guardian directory removal failed: {error}"));
    }
    if failures.is_empty() {
        String::new()
    } else {
        format!("; {}", failures.join("; "))
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
struct BoundedOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
async fn read_bounded_output<R>(mut reader: R, limit: usize) -> Result<BoundedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(4096));
    let mut overflowed = false;
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).await.map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                format!("cannot read version output: {error}"),
            )
        })?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let retained = remaining.min(count);
        output.extend_from_slice(&buffer[..retained]);
        if retained < count {
            overflowed = true;
            break;
        }
    }
    Ok(BoundedOutput {
        bytes: output,
        overflowed,
    })
}

fn canonical_json_sha256(bytes: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.schema",
            format!("embedded protocol schema is invalid JSON: {error}"),
        )
    })?;
    let mut canonical = Vec::with_capacity(bytes.len());
    write_canonical_json(&value, &mut canonical).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.schema",
            format!("cannot canonicalize protocol schema: {error}"),
        )
    })?;
    Ok(hash_bytes(&canonical))
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) -> serde_json::Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_writer(&mut *output, value)?;
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                write_canonical_json(&values[key], output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.hash",
            format!("cannot open {}: {error}", path.display()),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.hash",
                format!("cannot read {}: {error}", path.display()),
            )
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format_digest(hasher.finalize()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format_digest(hasher.finalize())
}

fn format_digest(digest: impl IntoIterator<Item = u8>) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    const TEST_SCRIPT: &str = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.test'; exit 0; fi\nexit 0\n";
    const HANG_SCRIPT: &str = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then sleep 5; fi\n";

    #[test]
    fn audited_locks_are_well_formed_and_match_embedded_schema() {
        let pinned = PinnedRuntime::load().expect("valid pinned runtime metadata");
        let lock = pinned.lock();
        let schema_hash = canonical_json_sha256(PINNED_SCHEMA).expect("canonical schema hash");

        assert_eq!(lock.version(), "0.149.1");
        assert_eq!(schema_hash, lock.protocol_schema_canonical_sha256());
    }

    #[test]
    fn canonical_schema_hash_ignores_object_key_order_but_not_values() {
        let first =
            canonical_json_sha256(br#"{"b":2,"a":{"y":4,"x":3}}"#).expect("first canonical hash");
        let reordered = canonical_json_sha256(br#"{"a":{"x":3,"y":4},"b":2}"#)
            .expect("reordered canonical hash");
        let changed =
            canonical_json_sha256(br#"{"a":{"x":3,"y":5},"b":2}"#).expect("changed canonical hash");
        assert_eq!(first, reordered);
        assert_ne!(first, changed);
    }

    #[test]
    fn rejects_parent_and_curdir_paths() {
        assert!(RuntimeArtifact::new("../codex", "0".repeat(64), true).is_err());
        assert!(RuntimeArtifact::new("./codex", "0".repeat(64), true).is_err());
    }

    #[test]
    fn rejects_non_executable_entrypoint_lock() {
        let artifacts =
            vec![RuntimeArtifact::new("bin/codex", "0".repeat(64), false).expect("artifact")];
        let error = RuntimeLock::new(
            "0.test",
            "0".repeat(40),
            "test-target",
            "codex",
            "bin/codex",
            "0".repeat(64),
            artifacts,
        )
        .expect_err("entrypoint must be executable");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_duplicate_artifact_paths() {
        let artifacts = vec![
            RuntimeArtifact::new("bin/codex", "0".repeat(64), true).expect("artifact"),
            RuntimeArtifact::new("bin/codex", "1".repeat(64), true).expect("artifact"),
        ];
        let error = RuntimeLock::new(
            "0.test",
            "0".repeat(40),
            "test-target",
            "codex",
            "bin/codex",
            "0".repeat(64),
            artifacts,
        )
        .expect_err("duplicate paths must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_incompatible_host_target() {
        let error = verify_host_target("x86_64-apple-darwin").expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn accepts_the_current_supported_host_target() {
        verify_host_target("aarch64-apple-darwin")
            .expect("current supported host must be accepted");
    }

    #[test]
    fn rejects_unexpected_file_and_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        fs::write(directory.path().join("unexpected"), "x").expect("unexpected file");
        assert_eq!(
            verify_filesystem(package)
                .expect_err("unexpected file")
                .kind(),
            ErrorKind::RuntimeVerification
        );

        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        fs::create_dir(directory.path().join("unexpected-dir")).expect("unexpected dir");
        assert_eq!(
            verify_filesystem(package)
                .expect_err("unexpected dir")
                .kind(),
            ErrorKind::RuntimeVerification
        );
    }

    #[test]
    fn rejects_missing_or_modified_artifact() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        fs::remove_file(directory.path().join("bin/codex")).expect("remove");
        assert_eq!(
            verify_filesystem(package)
                .expect_err("missing artifact")
                .kind(),
            ErrorKind::RuntimeVerification
        );

        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        fs::write(directory.path().join("bin/codex"), "modified").expect("modify");
        assert_eq!(
            verify_filesystem(package)
                .expect_err("modified artifact")
                .kind(),
            ErrorKind::RuntimeVerification
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_insecure_permissions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        let entrypoint = directory.path().join("bin/codex");
        let target = directory.path().join("bin/real-codex");
        fs::rename(&entrypoint, &target).expect("rename");
        symlink(&target, &entrypoint).expect("symlink");
        assert_eq!(
            verify_filesystem(package).expect_err("symlink").kind(),
            ErrorKind::RuntimeVerification
        );

        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        let entrypoint = directory.path().join("bin/codex");
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o644)).expect("mode");
        assert_eq!(
            verify_filesystem(package)
                .expect_err("non-executable")
                .kind(),
            ErrorKind::RuntimeVerification
        );

        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        let entrypoint = directory.path().join("bin/codex");
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o777)).expect("mode");
        assert_eq!(
            verify_filesystem(package)
                .expect_err("world writable")
                .kind(),
            ErrorKind::RuntimeVerification
        );
    }

    #[test]
    fn rejects_manifest_unknown_fields_and_identity_mismatch() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(
            directory.path(),
            TEST_SCRIPT,
            Some(
                r#"{"layoutVersion":1,"version":"0.test","target":"test-target","variant":"codex","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path","extra":true}"#,
            ),
        );
        assert_eq!(
            verify_filesystem(package)
                .expect_err("unknown manifest field")
                .kind(),
            ErrorKind::RuntimeVerification
        );

        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(
            directory.path(),
            TEST_SCRIPT,
            Some(
                r#"{"layoutVersion":1,"version":"wrong","target":"test-target","variant":"codex","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path"}"#,
            ),
        );
        assert_eq!(
            verify_filesystem(package)
                .expect_err("manifest mismatch")
                .kind(),
            ErrorKind::RuntimeVerification
        );
    }

    #[tokio::test]
    async fn version_timeout_terminates_hanging_process() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), HANG_SCRIPT, None);
        let verified = verify_filesystem(package).expect("filesystem verification");
        let error =
            verify_version_with_timeout(&verified.entrypoint, "0.test", Duration::from_millis(250))
                .await
                .expect_err("version must time out");
        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
        assert!(error.message().contains("terminated"));
    }

    fn create_test_package(
        root: &Path,
        script: &str,
        manifest_override: Option<&str>,
    ) -> RuntimePackage {
        let entrypoint = root.join("bin/codex");
        fs::create_dir_all(entrypoint.parent().expect("bin parent")).expect("bin");
        fs::create_dir_all(root.join("codex-path")).expect("path dir");
        fs::create_dir_all(root.join("codex-resources")).expect("resources dir");
        fs::write(&entrypoint, script).expect("script");
        #[cfg(unix)]
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o755)).expect("executable");

        let manifest = manifest_override.unwrap_or(
            r#"{"layoutVersion":1,"version":"0.test","target":"test-target","variant":"codex","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path"}"#,
        );
        let manifest_path = root.join("codex-package.json");
        fs::write(&manifest_path, manifest).expect("manifest");
        let lock = RuntimeLock::new(
            "0.test",
            "0".repeat(40),
            "test-target",
            "codex",
            "bin/codex",
            "0".repeat(64),
            vec![
                RuntimeArtifact::new("bin/codex", hash_file(&entrypoint).expect("hash"), true)
                    .expect("entry lock"),
                RuntimeArtifact::new(
                    "codex-package.json",
                    hash_file(&manifest_path).expect("hash"),
                    false,
                )
                .expect("manifest lock"),
            ],
        )
        .expect("runtime lock");
        RuntimePackage::new(root, lock)
    }
}
