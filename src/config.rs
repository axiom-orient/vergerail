//! Connection and safety configuration.

use crate::error::{Error, ErrorKind, Result};
use crate::runtime::RuntimePackage;
use std::time::{Duration, Instant};

const MIN_FRAME_BYTES: usize = 64 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Complete configuration required to connect to a pinned app-server runtime.
#[derive(Clone, Debug)]
pub struct CodexConfig {
    pub(crate) runtime: RuntimePackage,
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
    pub(crate) absolute_deadline: Option<Instant>,
}

impl CodexConfig {
    /// Smallest JSONL frame accepted by the app-server transport.
    pub const MIN_FRAME_BYTES: usize = MIN_FRAME_BYTES;

    /// Largest JSONL frame accepted by the app-server transport.
    pub const MAX_FRAME_BYTES: usize = MAX_FRAME_BYTES;

    /// Creates a configuration that reuses the standard Codex account home.
    #[must_use]
    pub fn new(runtime: RuntimePackage) -> Self {
        Self {
            runtime,
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
            absolute_deadline: None,
        }
    }

    /// Sets the client title reported during the initialize handshake.
    #[must_use]
    pub fn with_client_title(mut self, title: impl Into<String>) -> Self {
        self.client_title = title.into();
        self
    }

    /// Enables Codex image-generation sessions for this client.
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

    /// Sets one monotonic deadline shared by runtime verification, requests,
    /// operations, and shutdown. Intended for process-boundary adapters.
    #[must_use]
    pub fn with_absolute_deadline(mut self, deadline: Instant) -> Self {
        self.absolute_deadline = Some(deadline);
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
        if !(Self::MIN_FRAME_BYTES..=Self::MAX_FRAME_BYTES).contains(&bytes) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::with_max_frame_bytes",
                "frame limit must be between 64 KiB and 64 MiB",
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
        if !(Self::MIN_FRAME_BYTES..=Self::MAX_FRAME_BYTES).contains(&self.max_frame_bytes) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "CodexConfig::validate",
                "frame limit must be between 64 KiB and 64 MiB",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for_validation() -> CodexConfig {
        let runtime = RuntimePackage::pinned(".").expect("audited runtime lock");
        CodexConfig::new(runtime)
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
    fn validates_frame_limit_and_capacities() {
        let config = config_for_validation();
        assert_eq!(CodexConfig::MIN_FRAME_BYTES, 64 * 1024);
        assert_eq!(CodexConfig::MAX_FRAME_BYTES, 64 * 1024 * 1024);
        assert!(
            config
                .clone()
                .with_max_frame_bytes(CodexConfig::MIN_FRAME_BYTES)
                .is_ok()
        );
        assert!(
            config
                .clone()
                .with_max_frame_bytes(CodexConfig::MAX_FRAME_BYTES)
                .is_ok()
        );
        assert_eq!(
            config
                .clone()
                .with_max_frame_bytes(CodexConfig::MAX_FRAME_BYTES + 1)
                .expect_err("frame limit above canonical maximum")
                .kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.max_frame_bytes = CodexConfig::MIN_FRAME_BYTES - 1;
        assert_eq!(
            config.validate().expect_err("small frame limit").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.max_frame_bytes = CodexConfig::MAX_FRAME_BYTES + 1;
        assert_eq!(
            config.validate().expect_err("large frame limit").kind(),
            ErrorKind::InvalidInput
        );

        let mut config = config_for_validation();
        config.event_capacity = 0;
        assert_eq!(
            config.validate().expect_err("zero event capacity").kind(),
            ErrorKind::InvalidInput
        );
    }
}
