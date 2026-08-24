//! Embedded runtime identity and download metadata.

use crate::error::{Error, ErrorKind, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

const PINNED_RUNTIME_METADATA: &str = include_str!("../../runtime/pinned-macos-aarch64.json");

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PinnedRuntime {
    lock: RuntimeLock,
    download: PinnedRuntimeDownload,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedRuntimeMetadata {
    version: String,
    upstream_commit: String,
    target: String,
    variant: String,
    entrypoint: PathBuf,
    protocol_schema_canonical_sha256: String,
    download: PinnedRuntimeDownload,
    artifacts: Vec<PinnedRuntimeArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedRuntimeDownload {
    url: String,
    bytes: u64,
    sha256: String,
    archive_prefix: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedRuntimeArtifact {
    path: PathBuf,
    sha256: String,
    executable: bool,
}

impl PinnedRuntime {
    pub(crate) fn load() -> Result<Self> {
        let metadata: PinnedRuntimeMetadata = serde_json::from_str(PINNED_RUNTIME_METADATA)
            .map_err(|error| {
                Error::new(
                    ErrorKind::RuntimeVerification,
                    "runtime.metadata",
                    format!("invalid embedded runtime metadata: {error}"),
                )
            })?;
        let artifacts = metadata
            .artifacts
            .into_iter()
            .map(|artifact| {
                RuntimeArtifact::new(artifact.path, artifact.sha256, artifact.executable)
            })
            .collect::<Result<Vec<_>>>()?;
        let lock = RuntimeLock::new(
            metadata.version,
            metadata.upstream_commit,
            metadata.target,
            metadata.variant,
            metadata.entrypoint,
            metadata.protocol_schema_canonical_sha256,
            artifacts,
        )?;
        metadata.download.validate()?;
        Ok(Self {
            lock,
            download: metadata.download,
        })
    }

    pub(crate) fn lock(&self) -> &RuntimeLock {
        &self.lock
    }

    pub(crate) fn download(&self) -> &PinnedRuntimeDownload {
        &self.download
    }
}

impl PinnedRuntimeDownload {
    fn validate(&self) -> Result<()> {
        if !self.url.starts_with("https://")
            || self.url.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "runtime.download",
                "runtime download URL must be an absolute HTTPS URL without whitespace",
            ));
        }
        if self.bytes == 0 || self.bytes == u64::MAX {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "runtime.download",
                "runtime download byte count must be greater than zero and leave room for the overflow sentinel",
            ));
        }
        validate_sha256(&self.sha256)?;
        validate_relative_path(&self.archive_prefix)?;
        Ok(())
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn archive_prefix(&self) -> &Path {
        &self.archive_prefix
    }
}

/// One required file in a canonical Codex package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeArtifact {
    pub(crate) relative_path: PathBuf,
    pub(crate) sha256: String,
    pub(crate) executable: bool,
}

impl RuntimeArtifact {
    /// Defines a required artifact and its lowercase SHA-256 digest.
    pub(crate) fn new(
        relative_path: impl Into<PathBuf>,
        sha256: impl Into<String>,
        executable: bool,
    ) -> Result<Self> {
        let artifact = Self {
            relative_path: relative_path.into(),
            sha256: sha256.into(),
            executable,
        };
        validate_relative_path(&artifact.relative_path)?;
        validate_sha256(&artifact.sha256)?;
        Ok(artifact)
    }
}

/// Immutable identity expected from a Codex runtime package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeLock {
    pub(crate) version: String,
    pub(crate) upstream_commit: String,
    pub(crate) target: String,
    pub(crate) variant: String,
    pub(crate) entrypoint: PathBuf,
    pub(crate) protocol_schema_canonical_sha256: String,
    pub(crate) artifacts: Vec<RuntimeArtifact>,
}

impl RuntimeLock {
    /// Creates a runtime lock from explicit, audited values.
    pub(crate) fn new(
        version: impl Into<String>,
        upstream_commit: impl Into<String>,
        target: impl Into<String>,
        variant: impl Into<String>,
        entrypoint: impl Into<PathBuf>,
        protocol_schema_canonical_sha256: impl Into<String>,
        artifacts: Vec<RuntimeArtifact>,
    ) -> Result<Self> {
        let lock = Self {
            version: version.into(),
            upstream_commit: upstream_commit.into(),
            target: target.into(),
            variant: variant.into(),
            entrypoint: entrypoint.into(),
            protocol_schema_canonical_sha256: protocol_schema_canonical_sha256.into(),
            artifacts,
        };
        validate_relative_path(&lock.entrypoint)?;
        validate_sha256(&lock.protocol_schema_canonical_sha256)?;
        validate_commit_sha(&lock.upstream_commit)?;
        validate_runtime_identifier(&lock.version, "version")?;
        validate_runtime_identifier(&lock.target, "target")?;
        validate_runtime_identifier(&lock.variant, "variant")?;
        if lock.artifacts.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "RuntimeLock::new",
                "at least one runtime artifact is required",
            ));
        }
        let mut paths = HashSet::new();
        for artifact in &lock.artifacts {
            if !paths.insert(artifact.relative_path.clone()) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "RuntimeLock::new",
                    format!(
                        "runtime artifact path is duplicated: {}",
                        artifact.relative_path.display()
                    ),
                ));
            }
        }
        match lock
            .artifacts
            .iter()
            .find(|artifact| artifact.relative_path == lock.entrypoint)
        {
            Some(artifact) if artifact.executable => {}
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "RuntimeLock::new",
                    "entrypoint artifact must be executable",
                ));
            }
            None => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "RuntimeLock::new",
                    "entrypoint must also be present in artifacts",
                ));
            }
        }
        Ok(lock)
    }

    /// Returns the expected Codex version.
    #[must_use]
    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    /// Returns the exact upstream Codex commit used for this runtime.
    #[must_use]
    pub(crate) fn upstream_commit(&self) -> &str {
        &self.upstream_commit
    }

    /// Returns the expected Rust target triple used by the runtime package.
    #[must_use]
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    /// Returns the canonical JSON SHA-256 of the generated v2 schema associated with this runtime.
    #[must_use]
    pub(crate) fn protocol_schema_canonical_sha256(&self) -> &str {
        &self.protocol_schema_canonical_sha256
    }
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "runtime.path",
            "runtime paths must contain only non-empty relative normal components",
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "runtime.sha256",
            "SHA-256 values must contain exactly 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_commit_sha(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "runtime.commit",
            "upstream commit must contain exactly 40 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_runtime_identifier(value: &str, field: &str) -> Result<()> {
    if !(1..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "runtime.identifier",
            format!(
                "runtime {field} must contain 1-128 ASCII alphanumeric, dot, dash, or underscore characters"
            ),
        ));
    }
    Ok(())
}
