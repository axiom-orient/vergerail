//! Client connection lifecycle and causal disconnect ownership.

use crate::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Owns shutdown admission and the first causal disconnect error.
pub(crate) struct ConnectionLifecycle {
    disconnected: AtomicBool,
    closing: AtomicBool,
    failure: Mutex<Option<Error>>,
}

impl ConnectionLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            disconnected: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            failure: Mutex::new(None),
        }
    }

    fn failure_state(&self) -> MutexGuard<'_, Option<Error>> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Returns true only for the caller that starts graceful shutdown.
    pub(crate) fn begin_closing(&self) -> bool {
        !self.closing.swap(true, Ordering::AcqRel)
    }

    /// Publishes the causal error before exposing the disconnected flag.
    /// Returns true only for the caller that owns terminal cleanup.
    pub(crate) fn begin_disconnect(&self, error: Error) -> bool {
        let mut failure = self.failure_state();
        if self.disconnected.load(Ordering::Acquire) {
            return false;
        }
        *failure = Some(error);
        self.disconnected.store(true, Ordering::Release);
        true
    }

    pub(crate) fn failure(&self) -> Option<Error> {
        self.failure_state().clone()
    }

    /// Enriches the already-published causal error after process cleanup.
    pub(crate) fn replace_failure(&self, error: Error) {
        *self.failure_state() = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectionLifecycle;
    use crate::error::{Error, ErrorKind};

    #[test]
    fn first_disconnect_owns_cleanup_and_publishes_its_cause() {
        let lifecycle = ConnectionLifecycle::new();
        let first = Error::new(ErrorKind::OutcomeUnknown, "turn.start", "first");
        let later = Error::new(ErrorKind::Disconnected, "router", "later");

        assert!(lifecycle.begin_disconnect(first.clone()));
        assert!(!lifecycle.begin_disconnect(later));
        assert!(lifecycle.is_disconnected());
        assert_eq!(lifecycle.failure(), Some(first));
    }

    #[test]
    fn graceful_shutdown_has_one_owner() {
        let lifecycle = ConnectionLifecycle::new();
        assert!(lifecycle.begin_closing());
        assert!(!lifecycle.begin_closing());
        assert!(lifecycle.is_closing());
    }
}
