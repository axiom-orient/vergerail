//! Public error model.

use std::fmt;
use std::time::Duration;

/// Result type used by Vergerail.
pub type Result<T> = std::result::Result<T, Error>;

/// Stable error categories exposed by Vergerail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A caller supplied an invalid value or state transition.
    InvalidInput,
    /// The pinned Codex runtime package failed identity or layout checks.
    RuntimeVerification,
    /// The app-server process could not be started or controlled.
    Process,
    /// The JSONL or JSON-RPC contract was violated.
    Protocol,
    /// The app-server returned a JSON-RPC error.
    Rpc,
    /// A bounded operation exceeded its explicit deadline.
    Timeout,
    /// The process or transport disconnected.
    Disconnected,
    /// A non-idempotent request was written, but its outcome could not be observed.
    OutcomeUnknown,
    /// The consumer did not drain a bounded event stream fast enough.
    ConsumerLagged,
    /// A bounded in-memory resource exceeded its configured ceiling.
    ResourceLimit,
    /// Authentication did not complete successfully.
    Authentication,
    /// Shutdown completed with one or more observable failures.
    Shutdown,
}

/// A cloneable, redacted failure report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    operation: &'static str,
    message: String,
    rpc_code: Option<i64>,
    stderr_tail: Option<String>,
}

impl Error {
    /// Returns the stable category for this failure.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the operation that failed.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// Returns the redacted human-readable reason.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the JSON-RPC error code, when one was supplied by app-server.
    #[must_use]
    pub const fn rpc_code(&self) -> Option<i64> {
        self.rpc_code
    }

    /// Returns a bounded, redacted stderr suffix, when available.
    #[must_use]
    pub fn stderr_tail(&self) -> Option<&str> {
        self.stderr_tail.as_deref()
    }

    pub(crate) fn new(
        kind: ErrorKind,
        operation: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            operation,
            message: message.into(),
            rpc_code: None,
            stderr_tail: None,
        }
    }

    pub(crate) fn rpc(operation: &'static str, code: i64, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Rpc,
            operation,
            message: message.into(),
            rpc_code: Some(code),
            stderr_tail: None,
        }
    }

    pub(crate) fn timeout(operation: &'static str, timeout: Duration) -> Self {
        Self::new(
            ErrorKind::Timeout,
            operation,
            format!("operation exceeded {} ms", timeout.as_millis()),
        )
    }

    pub(crate) fn with_related_error(mut self, context: &str, related: &Self) -> Self {
        self.message.push_str("; ");
        self.message.push_str(context);
        self.message.push_str(": ");
        self.message.push_str(&related.to_string());
        if self.stderr_tail.is_none() {
            self.stderr_tail = related.stderr_tail.clone();
        }
        self
    }

    pub(crate) fn with_stderr(mut self, stderr_tail: Option<String>) -> Self {
        self.stderr_tail = stderr_tail;
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.message)?;
        if let Some(code) = self.rpc_code {
            write!(formatter, " (rpc code {code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_cleanup_error_preserves_primary_contract() {
        let primary = Error::rpc("turn.start", -32_000, "primary failure");
        let cleanup = Error::new(ErrorKind::Shutdown, "thread.unsubscribe", "cleanup failure")
            .with_stderr(Some("bounded cleanup stderr".to_owned()));

        let combined = primary.with_related_error("cleanup also failed", &cleanup);

        assert_eq!(combined.kind(), ErrorKind::Rpc);
        assert_eq!(combined.operation(), "turn.start");
        assert_eq!(combined.rpc_code(), Some(-32_000));
        assert!(combined.message().contains("primary failure"));
        assert!(combined.message().contains("cleanup also failed"));
        assert!(combined.message().contains("thread.unsubscribe"));
        assert_eq!(combined.stderr_tail(), Some("bounded cleanup stderr"));
    }

    #[test]
    fn related_cleanup_error_does_not_replace_primary_stderr() {
        let primary = Error::new(ErrorKind::Protocol, "initialize", "primary failure")
            .with_stderr(Some("primary stderr".to_owned()));
        let cleanup = Error::new(ErrorKind::Shutdown, "shutdown", "cleanup failure")
            .with_stderr(Some("cleanup stderr".to_owned()));

        let combined = primary.with_related_error("cleanup also failed", &cleanup);

        assert_eq!(combined.stderr_tail(), Some("primary stderr"));
    }
}
