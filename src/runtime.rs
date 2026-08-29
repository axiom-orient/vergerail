//! Verification of a pinned canonical Codex package.

use crate::error::{Error, ErrorKind, Result};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::private::process_tree;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use tokio::io::{AsyncRead, AsyncReadExt as _};
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
use tokio::process::Child;
use tokio::task::JoinHandle;
use tokio::time::timeout;

mod lock;
mod manager;

pub(crate) use lock::{PinnedRuntime, PinnedRuntimeDownload, RuntimeArtifact, RuntimeLock};
pub use manager::{DownloadPolicy, ResolvedRuntime, RuntimeOrigin, RuntimeResolver};

const PINNED_SCHEMA: &[u8] =
    include_bytes!("../protocol/codex-0.150.1/codex_app_server_protocol.v2.schemas.json");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const VERSION_OUTPUT_LIMIT: usize = 8 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFICATION_CLEANUP_BUDGET: Duration = Duration::from_secs(2);
const VERIFICATION_READ_CHUNK_BYTES: usize = 128 * 1024;
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
const VERIFICATION_HELPER_ARGUMENT: &str = "--vergerail-runtime-verify";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const VERSION_CLEANUP_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
struct VerificationControl {
    cancelled: Arc<AtomicBool>,
    active_workers: Arc<AtomicUsize>,
    started_workers: Arc<AtomicUsize>,
    #[cfg(test)]
    checkpoint_delay: Option<Duration>,
}

impl VerificationControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            active_workers: Arc::new(AtomicUsize::new(0)),
            started_workers: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_delay: None,
        })
    }

    #[cfg(test)]
    fn slow_for_test(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            active_workers: Arc::new(AtomicUsize::new(0)),
            started_workers: Arc::new(AtomicUsize::new(0)),
            checkpoint_delay: Some(delay),
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn checkpoint(&self, operation: &'static str) -> Result<()> {
        #[cfg(test)]
        if let Some(delay) = self.checkpoint_delay {
            let started = Instant::now();
            while started.elapsed() < delay {
                if self.cancelled.load(Ordering::Acquire) {
                    return Err(Error::timeout(operation, Duration::ZERO));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if self.cancelled.load(Ordering::Acquire) {
            Err(Error::timeout(operation, Duration::ZERO))
        } else {
            Ok(())
        }
    }

    fn begin_worker(&self) -> VerificationWorkerGuard<'_> {
        self.started_workers.fetch_add(1, Ordering::Relaxed);
        self.active_workers.fetch_add(1, Ordering::AcqRel);
        VerificationWorkerGuard { control: self }
    }
}

struct VerificationWorkerGuard<'a> {
    control: &'a VerificationControl,
}

impl Drop for VerificationWorkerGuard<'_> {
    fn drop(&mut self) {
        self.control.active_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

struct VerificationJob {
    control: Arc<VerificationControl>,
    handle: Option<JoinHandle<Result<VerifiedRuntime>>>,
    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
    process: Option<VerificationProcess>,
}

impl VerificationJob {
    fn spawn(
        package: RuntimePackage,
        control: Arc<VerificationControl>,
        deadline: Option<Instant>,
        operation: &'static str,
    ) -> Result<Self> {
        if let Some(deadline) = deadline {
            remaining_deadline(deadline, operation)?;
        }

        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        {
            if deadline.is_some() {
                return VerificationProcess::spawn(package, Arc::clone(&control)).map(|process| {
                    Self {
                        control,
                        handle: None,
                        process: Some(process),
                    }
                });
            }

            let worker_control = Arc::clone(&control);
            let handle = tokio::task::spawn_blocking(move || {
                let _worker = worker_control.begin_worker();
                verify_filesystem_with_control(package, &worker_control)
            });
            Ok(Self {
                control,
                handle: Some(handle),
                process: None,
            })
        }

        #[cfg(any(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
        {
            let worker_control = Arc::clone(&control);
            let handle = tokio::task::spawn_blocking(move || {
                let _worker = worker_control.begin_worker();
                verify_filesystem_with_control(package, &worker_control)
            });
            Ok(Self {
                control,
                handle: Some(handle),
            })
        }
    }

    #[allow(unused_mut)]
    async fn join_with_deadline(
        mut self,
        deadline: Option<Instant>,
        operation: &'static str,
    ) -> Result<VerifiedRuntime> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        {
            if let Some(process) = self.process.take() {
                self.join_process_with_deadline(process, deadline, operation)
                    .await
            } else {
                self.join_blocking_with_deadline(deadline, operation).await
            }
        }

        #[cfg(any(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
        {
            self.join_blocking_with_deadline(deadline, operation).await
        }
    }

    async fn join_blocking_with_deadline(
        self,
        deadline: Option<Instant>,
        operation: &'static str,
    ) -> Result<VerifiedRuntime> {
        let control = self.control;
        let mut handle = self
            .handle
            .expect("verification blocking job must own its worker handle");
        let Some(deadline) = deadline else {
            return join_verification_handle(handle, operation).await;
        };
        let remaining = match remaining_deadline(deadline, operation) {
            Ok(remaining) => remaining,
            Err(error) => {
                control.cancel();
                let cleanup = timeout(VERIFICATION_CLEANUP_BUDGET, &mut handle).await;
                if cleanup.is_err() {
                    handle.abort();
                }
                return Err(error);
            }
        };
        match timeout(remaining, &mut handle).await {
            Ok(result) => join_verification_result(result, operation),
            Err(_) => {
                control.cancel();
                let cleanup = timeout(VERIFICATION_CLEANUP_BUDGET, &mut handle).await;
                if cleanup.is_err() {
                    handle.abort();
                    return Err(Error::new(
                        ErrorKind::RuntimeVerification,
                        operation,
                        format!(
                            "verification cancellation exceeded {} ms before acknowledgement",
                            VERIFICATION_CLEANUP_BUDGET.as_millis()
                        ),
                    ));
                }
                Err(Error::timeout(operation, remaining))
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
    async fn join_process_with_deadline(
        mut self,
        process: VerificationProcess,
        deadline: Option<Instant>,
        operation: &'static str,
    ) -> Result<VerifiedRuntime> {
        self.process = Some(process);
        let Some(deadline) = deadline else {
            return self.finish_process(operation).await;
        };
        let remaining = match remaining_deadline(deadline, operation) {
            Ok(remaining) => remaining,
            Err(error) => {
                self.control.cancel();
                let cleanup = self
                    .process
                    .as_mut()
                    .expect("verification process must remain owned")
                    .terminate()
                    .await;
                return Err(with_verification_cleanup(error, cleanup));
            }
        };
        match timeout(
            remaining,
            self.process
                .as_mut()
                .expect("verification process must remain owned")
                .child
                .wait(),
        )
        .await
        {
            Ok(Ok(status)) => self.finish_process_status(status, operation).await,
            Ok(Err(error)) => {
                let cleanup = self
                    .process
                    .as_mut()
                    .expect("verification process must remain owned")
                    .terminate()
                    .await;
                Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    operation,
                    format!("verification helper wait failed: {error}{cleanup}"),
                ))
            }
            Err(_) => {
                self.control.cancel();
                let cleanup = self
                    .process
                    .as_mut()
                    .expect("verification process must remain owned")
                    .terminate()
                    .await;
                let error = Error::timeout(operation, remaining);
                if cleanup.is_empty() {
                    Err(error)
                } else {
                    Err(Error::new(
                        ErrorKind::Timeout,
                        operation,
                        format!("{}{}", error.message(), cleanup),
                    ))
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
    async fn finish_process(&mut self, operation: &'static str) -> Result<VerifiedRuntime> {
        let status = match self
            .process
            .as_mut()
            .expect("verification process must remain owned")
            .child
            .wait()
            .await
        {
            Ok(status) => status,
            Err(error) => {
                let cleanup = self
                    .process
                    .as_mut()
                    .expect("verification process must remain owned")
                    .terminate()
                    .await;
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    operation,
                    format!("verification helper wait failed: {error}{cleanup}"),
                ));
            }
        };
        self.finish_process_status(status, operation).await
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
    async fn finish_process_status(
        &mut self,
        status: std::process::ExitStatus,
        operation: &'static str,
    ) -> Result<VerifiedRuntime> {
        let stderr = match timeout(
            VERSION_CLEANUP_GRACE,
            &mut self
                .process
                .as_mut()
                .expect("verification process must remain owned")
                .stderr_task,
        )
        .await
        {
            Ok(Ok(Ok(output))) if !output.overflowed => {
                String::from_utf8_lossy(&output.bytes).trim().to_owned()
            }
            Ok(Ok(Ok(_))) => "verification helper stderr exceeded its limit".to_owned(),
            Ok(Ok(Err(error))) => format!("verification helper stderr failed: {error}"),
            Ok(Err(error)) => format!("verification helper stderr task failed: {error}"),
            Err(_) => {
                self.process
                    .as_mut()
                    .expect("verification process must remain owned")
                    .stderr_task
                    .abort();
                "verification helper stderr remained open after exit".to_owned()
            }
        };
        let cleanup = self
            .process
            .as_ref()
            .expect("verification process must remain owned")
            .cleanup_artifacts();
        if !status.success() {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                operation,
                format!(
                    "verification helper exited with {status}: {}{}",
                    if stderr.is_empty() {
                        "no diagnostic"
                    } else {
                        &stderr
                    },
                    cleanup
                ),
            ));
        }
        if !stderr.is_empty() {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                operation,
                format!("verification helper emitted a diagnostic: {stderr}{cleanup}"),
            ));
        }
        if !cleanup.is_empty() {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.guardian",
                cleanup.trim_start_matches(';').trim().to_owned(),
            ));
        }
        let process = self
            .process
            .as_ref()
            .expect("verification process must remain owned");
        let root = process.package.root.canonicalize().map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                operation,
                format!("cannot canonicalize verified package root: {error}"),
            )
        })?;
        Ok(VerifiedRuntime {
            entrypoint: root.join(&process.package.lock.entrypoint),
            root,
            lock: process.package.lock.clone(),
        })
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
struct VerificationProcess {
    child: Child,
    identity: process_tree::ProcessIdentity,
    guardian_path: PathBuf,
    guardian_directory: PathBuf,
    stderr_task: JoinHandle<Result<BoundedOutput>>,
    package: RuntimePackage,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
impl VerificationProcess {
    fn spawn(package: RuntimePackage, _control: Arc<VerificationControl>) -> Result<Self> {
        let helper = env::current_exe().map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.verify.helper",
                format!("cannot locate packaged provider verifier: {error}"),
            )
        })?;
        if helper.file_stem().and_then(|name| name.to_str()) != Some("vergerail-upagent-provider") {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.verify.helper",
                "the packaged provider verifier is unavailable in the current executable",
            ));
        }
        let guardian_directory =
            process_tree::create_guardian_directory(&env::temp_dir(), "vergerail-runtime-verify")
                .map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.guardian",
                    format!("cannot create runtime verification guardian directory: {error}"),
                )
            })?;
        let guardian_path = match process_tree::extract_guardian(&guardian_directory) {
            Ok(path) => path,
            Err(error) => {
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.guardian",
                    format!("cannot materialize runtime verification guardian: {error}"),
                ));
            }
        };
        let root = if package.root.is_absolute() {
            package.root.clone()
        } else {
            match env::current_dir() {
                Ok(current) => current.join(&package.root),
                Err(error) => {
                    let _ = process_tree::remove_guardian(&guardian_path);
                    let _ = process_tree::remove_guardian_directory(&guardian_directory);
                    return Err(Error::new(
                        ErrorKind::RuntimeVerification,
                        "runtime.verify.helper",
                        format!("cannot resolve relative runtime package root: {error}"),
                    ));
                }
            }
        };
        let mut command = process_tree::command(&guardian_path, &helper);
        command
            .arg(VERIFICATION_HELPER_ARGUMENT)
            .arg(root)
            .env_clear()
            .current_dir(env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.verify.helper",
                    format!("failed to execute runtime verification guardian: {error}"),
                ));
            }
        };
        let identity = match process_tree::capture(&child) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = child.start_kill();
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.verify.helper",
                    format!("cannot capture runtime verification guardian identity: {error}"),
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = child.start_kill();
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.verify.helper",
                    "runtime verification guardian stderr was not piped",
                ));
            }
        };
        let stderr_task = tokio::spawn(read_bounded_output(stderr, VERSION_OUTPUT_LIMIT));
        Ok(Self {
            child,
            identity,
            guardian_path,
            guardian_directory,
            stderr_task,
            package,
        })
    }

    async fn terminate(&mut self) -> String {
        let kill_error = process_tree::terminate(self.identity, &mut self.child).err();
        self.stderr_task.abort();
        let wait = timeout(VERSION_CLEANUP_GRACE, self.child.wait()).await;
        let mut result = match (kill_error, wait) {
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
        result.push_str(&self.cleanup_artifacts());
        result
    }

    fn cleanup_artifacts(&self) -> String {
        cleanup_version_artifacts(&self.guardian_path, &self.guardian_directory)
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
fn with_verification_cleanup(error: Error, cleanup: String) -> Error {
    if cleanup.is_empty() {
        error
    } else {
        Error::new(
            error.kind(),
            error.operation(),
            format!("{}{}", error.message(), cleanup),
        )
    }
}

async fn join_verification_handle(
    handle: JoinHandle<Result<VerifiedRuntime>>,
    operation: &'static str,
) -> Result<VerifiedRuntime> {
    join_verification_result(handle.await, operation)
}

fn join_verification_result(
    result: std::result::Result<Result<VerifiedRuntime>, tokio::task::JoinError>,
    operation: &'static str,
) -> Result<VerifiedRuntime> {
    result.map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            operation,
            format!("verification worker failed: {error}"),
        )
    })?
}

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

    /// Verifies the package manifest, layout, permissions, sizes, and hashes
    /// without launching the app-server. The packaged UpAgent verifier uses
    /// this method inside the guardian-owned verification process.
    #[doc(hidden)]
    pub fn verify_filesystem(&self) -> Result<()> {
        verify_protocol_schema(&self.lock)?;
        verify_host_target(&self.lock.target)?;
        verify_filesystem(self.clone()).map(|_| ())
    }

    pub(crate) async fn verify(&self) -> Result<VerifiedRuntime> {
        self.verify_with_timeout(VERSION_TIMEOUT).await
    }

    pub(crate) async fn verify_with_deadline(&self, deadline: Instant) -> Result<VerifiedRuntime> {
        verify_protocol_schema(&self.lock)?;
        verify_host_target(&self.lock.target)?;

        let verified = VerificationJob::spawn(
            self.clone(),
            VerificationControl::new(),
            Some(deadline),
            "runtime.verify",
        )?
        .join_with_deadline(Some(deadline), "runtime.verify")
        .await?;

        let version_timeout = deadline.saturating_duration_since(Instant::now());
        if version_timeout.is_zero() {
            return Err(Error::timeout("runtime.version", Duration::ZERO));
        }
        verify_version_with_timeout(&verified.entrypoint, &self.lock.version, version_timeout)
            .await?;
        Ok(verified)
    }

    async fn verify_with_timeout(&self, version_timeout: Duration) -> Result<VerifiedRuntime> {
        verify_protocol_schema(&self.lock)?;
        verify_host_target(&self.lock.target)?;

        let verified = VerificationJob::spawn(
            self.clone(),
            VerificationControl::new(),
            None,
            "runtime.verify",
        )?
        .join_with_deadline(None, "runtime.verify")
        .await?;

        let remaining = version_timeout;
        verify_version_with_timeout(&verified.entrypoint, &self.lock.version, remaining).await?;
        Ok(verified)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedRuntime {
    pub(crate) root: PathBuf,
    pub(crate) entrypoint: PathBuf,
    pub(crate) lock: RuntimeLock,
}

impl VerifiedRuntime {
    #[cfg(test)]
    pub(crate) fn reverify_before_spawn(&self) -> Result<()> {
        let verified =
            verify_filesystem(RuntimePackage::new(self.root.clone(), self.lock.clone()))?;
        if verified.entrypoint != self.entrypoint {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.spawn",
                "runtime entrypoint changed after verification",
            ));
        }
        Ok(())
    }

    pub(crate) async fn reverify_before_spawn_with_deadline(
        &self,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let verified = VerificationJob::spawn(
            RuntimePackage::new(self.root.clone(), self.lock.clone()),
            VerificationControl::new(),
            deadline,
            "runtime.spawn",
        )?
        .join_with_deadline(deadline, "runtime.spawn")
        .await?;
        if let Some(deadline) = deadline {
            remaining_deadline(deadline, "runtime.spawn")?;
        }
        if verified.entrypoint != self.entrypoint {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.spawn",
                "runtime entrypoint changed after verification",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    async fn reverify_before_spawn_with_test_control(
        &self,
        deadline: Instant,
        control: Arc<VerificationControl>,
    ) -> Result<()> {
        let verified = VerificationJob::spawn(
            RuntimePackage::new(self.root.clone(), self.lock.clone()),
            control,
            Some(deadline),
            "runtime.spawn",
        )?
        .join_with_deadline(Some(deadline), "runtime.spawn")
        .await?;
        if verified.entrypoint != self.entrypoint {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.spawn",
                "runtime entrypoint changed after verification",
            ));
        }
        Ok(())
    }
}

fn remaining_deadline(deadline: Instant, operation: &'static str) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(Error::timeout(operation, Duration::ZERO))
    } else {
        Ok(remaining)
    }
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
    verify_filesystem_with_control(package, &VerificationControl::new())
}

fn verify_filesystem_with_control(
    package: RuntimePackage,
    control: &VerificationControl,
) -> Result<VerifiedRuntime> {
    control.checkpoint("runtime.verify")?;
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
    verify_exact_file_set(&root, &package.lock.artifacts, control)?;

    for artifact in &package.lock.artifacts {
        control.checkpoint("runtime.artifact")?;
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
        let actual = hash_file_with_control(&canonical, artifact.max_bytes, control)?;
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

    control.checkpoint("runtime.manifest")?;
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
        lock: package.lock,
    })
}

fn verify_exact_file_set(
    root: &Path,
    artifacts: &[RuntimeArtifact],
    control: &VerificationControl,
) -> Result<()> {
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
        control.checkpoint("runtime.layout")?;
        for entry in fs::read_dir(&directory).map_err(|error| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.layout",
                format!("cannot read {}: {error}", directory.display()),
            )
        })? {
            control.checkpoint("runtime.layout")?;
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

async fn verify_version_with_timeout(
    entrypoint: &Path,
    version: &str,
    version_timeout: Duration,
) -> Result<()> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (entrypoint, version, version_timeout);
        Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            "the guardian runtime is supported only on aarch64 macOS",
        ))
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
    let bundled_path = entrypoint
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("codex-path"))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.version",
                "runtime entrypoint has no canonical package root",
            )
        })?;
    let path = env::join_paths([bundled_path]).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.version",
            format!("cannot construct version probe PATH: {error}"),
        )
    })?;
    command
        .arg("--version")
        .env_clear()
        .env("PATH", path)
        .current_dir(std::env::temp_dir())
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

#[cfg(test)]
fn hash_file(path: &Path) -> Result<String> {
    hash_file_with_control(path, u64::MAX, &VerificationControl::new())
}

fn hash_file_with_control(
    path: &Path,
    max_bytes: u64,
    control: &VerificationControl,
) -> Result<String> {
    control.checkpoint("runtime.hash")?;
    let expected_metadata = fs::symlink_metadata(path).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.hash",
            format!("cannot inspect {}: {error}", path.display()),
        )
    })?;
    let expected_size = expected_metadata.len();
    if expected_size > max_bytes {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.size",
            format!(
                "{} is {} bytes, exceeding the locked {} byte ceiling",
                path.display(),
                expected_size,
                max_bytes
            ),
        ));
    }
    let mut file = File::open(path).map_err(|error| {
        Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.hash",
            format!("cannot open {}: {error}", path.display()),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; VERIFICATION_READ_CHUNK_BYTES];
    let mut total = 0_u64;
    loop {
        control.checkpoint("runtime.hash")?;
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
        total = total.checked_add(count as u64).ok_or_else(|| {
            Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.size",
                format!("{} size counter overflowed", path.display()),
            )
        })?;
        if total > max_bytes {
            return Err(Error::new(
                ErrorKind::RuntimeVerification,
                "runtime.size",
                format!(
                    "{} exceeded the locked {} byte ceiling while hashing",
                    path.display(),
                    max_bytes
                ),
            ));
        }
        hasher.update(&buffer[..count]);
    }
    if total != expected_size {
        return Err(Error::new(
            ErrorKind::RuntimeVerification,
            "runtime.size",
            format!(
                "{} changed size during hashing: expected {}, observed {}",
                path.display(),
                expected_size,
                total
            ),
        ));
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
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    const HANG_SCRIPT: &str = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then sleep 5; fi\n";

    #[test]
    fn audited_locks_are_well_formed_and_match_embedded_schema() {
        let pinned = PinnedRuntime::load().expect("valid pinned runtime metadata");
        let lock = pinned.lock();
        let schema_hash = canonical_json_sha256(PINNED_SCHEMA).expect("canonical schema hash");

        assert_eq!(lock.version(), "0.150.1");
        assert_eq!(schema_hash, lock.protocol_schema_canonical_sha256());
        let expected = [
            ("bin/codex", 228_986_048),
            ("bin/codex-code-mode-host", 57_150_064),
            ("codex-package.json", 200),
            ("codex-path/rg", 4_030_432),
            ("codex-resources/zsh/bin/zsh", 754_208),
        ];
        for (path, size) in expected {
            let artifact = lock
                .artifacts
                .iter()
                .find(|artifact| artifact.relative_path == Path::new(path))
                .expect("pinned artifact is present");
            assert_eq!(artifact.max_bytes, size, "{path} ceiling");
            assert!(artifact.max_bytes >= 1);
        }
        assert!(pinned.download().bytes() < lock.artifacts[0].max_bytes);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn installed_pinned_package_matches_observed_lock_sizes_and_hashes() {
        let root = env::var_os("VERGERAIL_CODEX_PACKAGE")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME").map(|home| {
                    PathBuf::from(home)
                        .join("Library/Application Support/vergerail/runtimes/codex/0.150.1/aarch64-apple-darwin")
                })
            });
        let Some(root) = root else {
            eprintln!("skipping installed pinned runtime proof: HOME is not set");
            return;
        };
        if !root.is_dir() {
            eprintln!(
                "skipping installed pinned runtime proof: package is not installed at {}",
                root.display()
            );
            return;
        }

        let package = RuntimePackage::pinned(&root).expect("installed package uses pinned lock");
        for artifact in &package.lock.artifacts {
            let path = root.join(&artifact.relative_path);
            let metadata = fs::symlink_metadata(&path).expect("locked installed artifact");
            assert_eq!(
                metadata.len(),
                artifact.max_bytes,
                "{} size",
                path.display()
            );
        }
        verify_filesystem(package).expect("installed pinned package verifies byte-for-byte");
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

    #[test]
    fn rejects_artifact_that_exceeds_its_locked_byte_ceiling_before_hashing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut package = create_test_package(directory.path(), TEST_SCRIPT, None);
        package.lock.artifacts[0].max_bytes = 1;

        let error = verify_filesystem(package).expect_err("oversized artifact");
        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
        assert_eq!(error.operation(), "runtime.size");
    }

    #[test]
    fn launch_reverification_rejects_mutation_after_initial_verification() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        let verified = verify_filesystem(package).expect("initial filesystem verification");

        fs::write(verified.root.join("bin/codex"), "mutated").expect("mutate entrypoint");
        let error = verified
            .reverify_before_spawn()
            .expect_err("launch must reverify the locked file set");
        assert_eq!(error.kind(), ErrorKind::RuntimeVerification);
        assert_eq!(error.operation(), "runtime.hash");
    }

    #[tokio::test]
    async fn launch_reverification_rejects_an_expired_deadline_before_spawn() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        let verified = verify_filesystem(package).expect("initial filesystem verification");
        let deadline = Instant::now() - Duration::from_millis(1);

        let error = verified
            .reverify_before_spawn_with_deadline(Some(deadline))
            .await
            .expect_err("expired launch deadline must prevent spawn");
        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert_eq!(error.operation(), "runtime.spawn");
    }

    #[tokio::test]
    async fn started_slow_reverification_cancels_and_joins_before_deadline_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let package = create_test_package(directory.path(), TEST_SCRIPT, None);
        let verified = verify_filesystem(package).expect("initial filesystem verification");
        let control = VerificationControl::slow_for_test(Duration::from_millis(250));
        let deadline = Instant::now() + Duration::from_millis(25);
        let spawn_attempts_before = crate::private::process::test_spawn_attempts();

        let error = verified
            .reverify_before_spawn_with_test_control(deadline, Arc::clone(&control))
            .await
            .expect_err("slow verification must honor its deadline");

        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert_eq!(error.operation(), "runtime.spawn");
        assert_eq!(control.started_workers.load(Ordering::Acquire), 1);
        assert_eq!(control.active_workers.load(Ordering::Acquire), 0);
        assert_eq!(
            crate::private::process::test_spawn_attempts(),
            spawn_attempts_before,
            "expired revalidation must not attempt process spawn"
        );
        assert!(
            control.cancelled.load(Ordering::Acquire),
            "deadline must signal the owned verification worker"
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

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
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
