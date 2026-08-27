//! Connection and safety configuration.

mod home;

pub(crate) use home::ManagedHome;

use crate::error::{Error, ErrorKind, Result};
use crate::runtime::RuntimePackage;
use std::path::PathBuf;
use std::time::Duration;

/// Complete configuration required to connect to a pinned app-server runtime.
#[derive(Clone, Debug)]
pub struct CodexConfig {
    pub(crate) runtime: RuntimePackage,
    pub(crate) codex_home: PathBuf,
    pub(crate) home_owner: String,
    pub(crate) client_name: String,
    pub(crate) client_title: String,
    pub(crate) request_timeout: Duration,
    pub(crate) approval_timeout: Duration,
    pub(crate) login_timeout: Duration,
    pub(crate) shutdown_timeout: Duration,
    pub(crate) max_frame_bytes: usize,
    pub(crate) outbound_capacity: usize,
    pub(crate) event_capacity: usize,
    pub(crate) stderr_capacity: usize,
    pub(crate) image_generation: bool,
}

impl CodexConfig {
    /// Creates a safe, deterministic configuration using a dedicated Codex home.
    #[must_use]
    pub fn new(runtime: RuntimePackage, codex_home: impl Into<PathBuf>) -> Self {
        Self {
            runtime,
            codex_home: codex_home.into(),
            home_owner: "vergerail".to_owned(),
            client_name: "vergerail".to_owned(),
            client_title: "Vergerail".to_owned(),
            request_timeout: Duration::from_secs(30),
            approval_timeout: Duration::from_secs(300),
            login_timeout: Duration::from_secs(600),
            shutdown_timeout: Duration::from_secs(10),
            max_frame_bytes: 16 * 1024 * 1024,
            outbound_capacity: 128,
            event_capacity: 256,
            stderr_capacity: 64 * 1024,
            image_generation: false,
        }
    }

    /// Binds the dedicated Codex home to one consuming application.
    ///
    /// A home first opened with one owner cannot later be opened by a different
    /// owner, even when both applications use Vergerail.
    #[must_use]
    pub fn with_home_owner(mut self, owner: impl Into<String>) -> Self {
        self.home_owner = owner.into();
        self
    }

    /// Sets the client title reported during the initialize handshake.
    #[must_use]
    pub fn with_client_title(mut self, title: impl Into<String>) -> Self {
        self.client_title = title.into();
        self
    }

    /// Enables Codex image-generation tools in this dedicated managed home.
    ///
    /// The capability is disabled by default. Enabling it permits the pinned
    /// app-server to call its image-generation model; generated image bytes are
    /// still bounded by each session's maximum retained output limit.
    #[must_use]
    pub const fn with_image_generation(mut self, enabled: bool) -> Self {
        self.image_generation = enabled;
        self
    }

    /// Sets the deadline for bounded control-plane requests.
    #[must_use]
    pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Sets the deadline for resolving reverse approval requests.
    #[must_use]
    pub const fn with_approval_timeout(mut self, timeout: Duration) -> Self {
        self.approval_timeout = timeout;
        self
    }

    /// Sets the deadline for completing managed ChatGPT login.
    #[must_use]
    pub const fn with_login_timeout(mut self, timeout: Duration) -> Self {
        self.login_timeout = timeout;
        self
    }

    /// Sets the deadline for graceful app-server shutdown.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Sets the maximum accepted or emitted JSONL frame size.
    pub fn with_max_frame_bytes(mut self, bytes: usize) -> Result<Self> {
        if bytes < 64 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::with_max_frame_bytes",
                "frame limit must be at least 64 KiB",
            ));
        }
        self.max_frame_bytes = bytes;
        Ok(self)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.client_name.trim().is_empty() || self.client_title.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::validate",
                "client name and title must be non-empty",
            ));
        }
        if self.codex_home.as_os_str().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::validate",
                "Codex home must be non-empty",
            ));
        }
        if !valid_home_owner(&self.home_owner) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::validate",
                "home owner must be 1..=64 lowercase ASCII letters, digits, or hyphens and must start and end with a letter or digit",
            ));
        }
        for (name, value) in [
            ("request_timeout", self.request_timeout),
            ("approval_timeout", self.approval_timeout),
            ("login_timeout", self.login_timeout),
            ("shutdown_timeout", self.shutdown_timeout),
        ] {
            if value.is_zero() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "CodexConfig::validate",
                    format!("{name} must be greater than zero"),
                ));
            }
        }
        if self.max_frame_bytes < 64 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::validate",
                "frame limit must be at least 64 KiB",
            ));
        }
        for (name, value) in [
            ("outbound_capacity", self.outbound_capacity),
            ("event_capacity", self.event_capacity),
            ("stderr_capacity", self.stderr_capacity),
        ] {
            if value == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "CodexConfig::validate",
                    format!("{name} must be greater than zero"),
                ));
            }
        }
        Ok(())
    }
}

fn valid_home_owner(owner: &str) -> bool {
    let bytes = owner.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for_validation() -> CodexConfig {
        let runtime = RuntimePackage::pinned(".").expect("audited runtime lock");
        CodexConfig::new(runtime, "vergerail-home")
    }

    #[test]
    fn rejects_zero_duration_deadlines() {
        let mut config = config_for_validation();
        config.request_timeout = Duration::ZERO;
        assert_eq!(
            config.validate().expect_err("zero request timeout").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.approval_timeout = Duration::ZERO;
        assert_eq!(
            config.validate().expect_err("zero approval timeout").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.login_timeout = Duration::ZERO;
        assert_eq!(
            config.validate().expect_err("zero login timeout").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.shutdown_timeout = Duration::ZERO;
        assert_eq!(
            config.validate().expect_err("zero shutdown timeout").kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn validates_home_frame_limit_and_capacities() {
        let mut config = config_for_validation();
        config.codex_home = PathBuf::new();
        assert_eq!(
            config.validate().expect_err("empty home").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.max_frame_bytes = 64 * 1024 - 1;
        assert_eq!(
            config.validate().expect_err("small frame limit").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.event_capacity = 0;
        assert_eq!(
            config.validate().expect_err("zero event capacity").kind(),
            ErrorKind::InvalidInput
        );

        let config = config_for_validation().with_home_owner("other_owner");
        assert_eq!(
            config.validate().expect_err("invalid home owner").kind(),
            ErrorKind::InvalidInput
        );
    }
}
