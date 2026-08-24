//! Pending JSON-RPC request ownership and cancellation state.

use crate::error::{Error, ErrorKind, Result};
use crate::private::wire::RpcId;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::oneshot;

pub(crate) struct RequestRegistry {
    next_id: AtomicU64,
    pending: Mutex<HashMap<RpcId, PendingRequest>>,
}

struct PendingRequest {
    operation: &'static str,
    non_idempotent: bool,
    dispatched: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    successful_response: Arc<AtomicBool>,
    sender: oneshot::Sender<Result<Value>>,
}

pub(crate) struct RequestRegistration {
    pub(crate) numeric_id: u64,
    pub(crate) id: RpcId,
    pub(crate) receiver: oneshot::Receiver<Result<Value>>,
    pub(crate) dispatched: Arc<AtomicBool>,
    pub(crate) cancellation: RequestCancellation,
}

#[derive(Clone)]
pub(crate) struct RequestCancellation {
    id: RpcId,
    operation: &'static str,
    non_idempotent: bool,
    dispatched: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    successful_response: Arc<AtomicBool>,
}

pub(crate) enum ResponseCompletion {
    Delivered,
    Missing,
    OrphanedSuccess { operation: &'static str },
}

pub(crate) enum TimeoutDisposition {
    TimedOut,
    OutcomeUnknown,
}

impl RequestRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<RpcId, PendingRequest>> {
        // Pending request bookkeeping is in-memory only. The guard is released
        // before channel delivery, process I/O, or any await point.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn register(
        &self,
        operation: &'static str,
        non_idempotent: bool,
    ) -> Result<RequestRegistration> {
        let numeric_id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current <= i64::MAX as u64).then(|| current + 1)
            })
            .map_err(|_| {
                Error::new(ErrorKind::Protocol, operation, "request id space exhausted")
            })?;
        let id = RpcId::Number(numeric_id as i64);
        let (sender, receiver) = oneshot::channel();
        let dispatched = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let successful_response = Arc::new(AtomicBool::new(false));
        let cancellation = RequestCancellation {
            id: id.clone(),
            operation,
            non_idempotent,
            dispatched: Arc::clone(&dispatched),
            cancelled: Arc::clone(&cancelled),
            successful_response: Arc::clone(&successful_response),
        };
        self.pending().insert(
            id.clone(),
            PendingRequest {
                operation,
                non_idempotent,
                dispatched: Arc::clone(&dispatched),
                cancelled,
                successful_response,
                sender,
            },
        );
        Ok(RequestRegistration {
            numeric_id,
            id,
            receiver,
            dispatched,
            cancellation,
        })
    }

    pub(crate) fn operation(&self, id: &RpcId) -> Option<&'static str> {
        self.pending().get(id).map(|pending| pending.operation)
    }

    pub(crate) fn remove(&self, id: &RpcId) {
        self.pending().remove(id);
    }

    pub(crate) fn cancel(&self, cancellation: &RequestCancellation) -> bool {
        let pending_at_cancellation = self.pending().remove(&cancellation.id).is_some();
        non_idempotent_outcome_may_be_orphaned(
            cancellation.non_idempotent,
            cancellation.dispatched.load(Ordering::Acquire),
            pending_at_cancellation,
            cancellation.successful_response.load(Ordering::Acquire),
            true,
        )
    }

    pub(crate) fn timeout(&self, cancellation: &RequestCancellation) -> TimeoutDisposition {
        let pending = self.pending().remove(&cancellation.id);
        let remote_outcome_unknown = pending.as_ref().is_some_and(|request| {
            request.non_idempotent && request.dispatched.load(Ordering::Acquire)
        }) || (cancellation.non_idempotent
            && cancellation.dispatched.load(Ordering::Acquire)
            && cancellation.successful_response.load(Ordering::Acquire));
        if remote_outcome_unknown {
            TimeoutDisposition::OutcomeUnknown
        } else {
            TimeoutDisposition::TimedOut
        }
    }

    fn take_for_completion(&self, id: &RpcId, response_succeeded: bool) -> Option<PendingRequest> {
        let mut requests = self.pending();
        let request = requests.get(id)?;
        if response_succeeded {
            // Publish success while the request is still protected by the
            // registry mutex. A racing timeout must observe either the
            // pending non-idempotent request or this success marker.
            request.successful_response.store(true, Ordering::Release);
        }
        requests.remove(id)
    }

    pub(crate) fn complete(&self, id: &RpcId, result: Result<Value>) -> ResponseCompletion {
        let response_succeeded = result.is_ok();
        let Some(pending) = self.take_for_completion(id, response_succeeded) else {
            return ResponseCompletion::Missing;
        };
        let operation = pending.operation;
        let non_idempotent = pending.non_idempotent;
        let dispatched = pending.dispatched.load(Ordering::Acquire);
        let cancelled = Arc::clone(&pending.cancelled);
        let response_handoff_failed =
            pending.sender.send(result).is_err() || cancelled.load(Ordering::Acquire);
        if non_idempotent_outcome_may_be_orphaned(
            non_idempotent,
            dispatched,
            false,
            response_succeeded,
            response_handoff_failed,
        ) {
            ResponseCompletion::OrphanedSuccess { operation }
        } else {
            ResponseCompletion::Delivered
        }
    }

    pub(crate) fn fail_all(&self, error: &Error) {
        let pending = std::mem::take(&mut *self.pending());
        for (_, request) in pending {
            let failure = if request.non_idempotent && request.dispatched.load(Ordering::Acquire) {
                Error::new(
                    ErrorKind::OutcomeUnknown,
                    request.operation,
                    "app-server disconnected after the request was written; outcome is unknown and was not retried",
                )
                .with_stderr(error.stderr_tail().map(str::to_owned))
            } else {
                Error::new(
                    ErrorKind::Disconnected,
                    request.operation,
                    error.message().to_owned(),
                )
                .with_stderr(error.stderr_tail().map(str::to_owned))
            };
            let _ = request.sender.send(Err(failure));
        }
    }
}

impl RequestCancellation {
    pub(crate) fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) const fn operation(&self) -> &'static str {
        self.operation
    }
}

fn non_idempotent_outcome_may_be_orphaned(
    non_idempotent: bool,
    dispatched: bool,
    pending_at_cancellation: bool,
    successful_response: bool,
    response_handoff_failed: bool,
) -> bool {
    non_idempotent
        && dispatched
        && (pending_at_cancellation || (successful_response && response_handoff_failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatched_non_idempotent_request_cancelled_while_pending_is_unknown() {
        assert!(non_idempotent_outcome_may_be_orphaned(
            true, true, true, false, true
        ));
    }

    #[test]
    fn successful_non_idempotent_response_lost_after_pending_removal_is_unknown() {
        assert!(non_idempotent_outcome_may_be_orphaned(
            true, true, false, true, true
        ));
    }

    #[test]
    fn known_failure_response_does_not_orphan_a_remote_success() {
        assert!(!non_idempotent_outcome_may_be_orphaned(
            true, true, false, false, true
        ));
    }

    #[test]
    fn idempotent_or_undispatched_requests_do_not_force_unknown_outcome() {
        assert!(!non_idempotent_outcome_may_be_orphaned(
            false, true, true, true, true
        ));
        assert!(!non_idempotent_outcome_may_be_orphaned(
            true, false, true, true, true
        ));
    }

    #[test]
    fn completion_transition_publishes_success_before_removing_registry_ownership() {
        let registry = RequestRegistry::new();
        let registration = registry
            .register("thread.start", true)
            .expect("registration");
        registration.dispatched.store(true, Ordering::Release);
        let cancellation = registration.cancellation.clone();

        let pending = registry
            .take_for_completion(&registration.id, true)
            .expect("pending request");
        assert!(
            cancellation.successful_response.load(Ordering::Acquire),
            "successful completion must be visible once registry ownership is released"
        );
        drop(pending);
        assert!(matches!(
            registry.timeout(&cancellation),
            TimeoutDisposition::OutcomeUnknown
        ));
    }

    #[test]
    fn successful_response_removed_before_timeout_is_still_outcome_unknown() {
        let registry = RequestRegistry::new();
        let registration = registry
            .register("thread.start", true)
            .expect("registration");
        registration.dispatched.store(true, Ordering::Release);
        let cancellation = registration.cancellation.clone();
        assert!(matches!(
            registry.complete(
                &registration.id,
                Ok(serde_json::json!({"thread": {"id": "t"}})),
            ),
            ResponseCompletion::Delivered
        ));

        assert!(matches!(
            registry.timeout(&cancellation),
            TimeoutDisposition::OutcomeUnknown
        ));
    }

    #[test]
    fn known_failure_removed_before_timeout_remains_a_plain_timeout() {
        let registry = RequestRegistry::new();
        let registration = registry
            .register("thread.start", true)
            .expect("registration");
        registration.dispatched.store(true, Ordering::Release);
        let cancellation = registration.cancellation.clone();
        assert!(matches!(
            registry.complete(
                &registration.id,
                Err(Error::rpc("thread.start", -32000, "rejected")),
            ),
            ResponseCompletion::Delivered
        ));

        assert!(matches!(
            registry.timeout(&cancellation),
            TimeoutDisposition::TimedOut
        ));
    }

    #[test]
    fn successful_handoff_followed_by_caller_abandonment_is_unknown() {
        let registry = RequestRegistry::new();
        let registration = registry
            .register("thread.start", true)
            .expect("registration");
        registration.dispatched.store(true, Ordering::Release);
        let cancellation = registration.cancellation.clone();
        let completion = registry.complete(
            &registration.id,
            Ok(serde_json::json!({"thread": {"id": "t"}})),
        );
        assert!(matches!(completion, ResponseCompletion::Delivered));

        cancellation.mark_cancelled();
        assert!(registry.cancel(&cancellation));
    }

    #[test]
    fn request_id_exhaustion_does_not_wrap() {
        let registry = RequestRegistry {
            next_id: AtomicU64::new(i64::MAX as u64),
            pending: Mutex::new(HashMap::new()),
        };
        let final_id = registry
            .register("rpc.test", false)
            .expect("final request id");
        assert_eq!(final_id.numeric_id, i64::MAX as u64);
        registry.remove(&final_id.id);

        let error = match registry.register("rpc.test", false) {
            Ok(_) => panic!("id space wrapped after exhaustion"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }
}
