//! Active turn ownership, event routing, and terminal recovery.

use crate::error::{Error, ErrorKind, Result};
use crate::event::{Event, RunResult, TurnCompletion, Usage};
use crate::image::ImageGeneration;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};

const RUN_INTERRUPT_STARTED: u8 = 1 << 0;
const RUN_PROVIDER_TERMINAL: u8 = 1 << 1;

/// In-process ownership for active provider turns keyed by thread identifier.
pub(crate) struct RunRegistry {
    routes: Mutex<HashMap<String, RunRoute>>,
}

struct RunRoute {
    events: mpsc::Sender<Event>,
    terminal: watch::Sender<Option<Result<RunResult>>>,
    active: Arc<AtomicBool>,
    abandoned: Arc<AtomicBool>,
    control: Arc<RunControl>,
    phase: RunPhase,
    pending_failure: Option<Error>,
    text: String,
    maximum_output_bytes: usize,
    usage: Option<Usage>,
    image_generations: Vec<ImageGeneration>,
    image_generation_bytes: usize,
    deferred_image_bytes: usize,
    deferred_image_bytes_by_id: HashMap<String, usize>,
}

enum RunPhase {
    Starting {
        deferred: VecDeque<DeferredRunNotification>,
    },
    Replaying {
        turn_id: String,
        deferred: VecDeque<DeferredRunNotification>,
    },
    Active {
        turn_id: String,
    },
    TerminalBeforeStart {
        turn_id: Option<String>,
    },
}

impl RunPhase {
    fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Replaying { turn_id, .. } | Self::Active { turn_id } => Some(turn_id),
            Self::TerminalBeforeStart {
                turn_id: Some(turn_id),
            } => Some(turn_id),
            Self::Starting { .. } | Self::TerminalBeforeStart { turn_id: None } => None,
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::TerminalBeforeStart { .. })
    }
}

pub(crate) enum DeferredRunNotification {
    Event {
        turn_id: String,
        source_method: String,
        event: Box<Event>,
    },
    Terminal(Value),
}

pub(crate) enum PreStartFailureTransition {
    Applied,
    Ignored,
    ProviderTurnOwned,
}

pub(crate) enum StartTurnTransition {
    Replay(VecDeque<DeferredRunNotification>),
    CompletedBeforeAcknowledgement,
    MissingRoute,
    TerminalTurnMismatch { expected: String },
    AlreadyAcknowledged,
}

pub(crate) enum ReplayTransition {
    Next(VecDeque<DeferredRunNotification>),
    Active,
    Stopped,
}

pub(crate) enum TerminalRouteOutcome {
    Deferred,
    Completed,
    Unregistered,
    Duplicate { turn_id: String },
    TurnMismatch { expected: String, observed: String },
}

pub(crate) enum RunEventOutcome {
    Unregistered,
    Delivered,
    Deferred,
    IgnoredBeforeTurn,
    IgnoredAfterAbandon,
    IgnoredAfterFailure,
    TurnMismatch {
        expected: String,
        observed: String,
    },
    AfterTerminal,
    RouteFailure {
        turn_id: String,
        control: Arc<RunControl>,
    },
}

impl RunRoute {
    fn route_event(
        &mut self,
        observed_turn_id: Option<&str>,
        source_method: &str,
        event: Event,
        defer_while_starting: bool,
        event_capacity: usize,
    ) -> RunEventOutcome {
        if self.pending_failure.is_some() {
            drop(event);
            return RunEventOutcome::IgnoredAfterFailure;
        }

        if defer_while_starting {
            let deferred_image_id = match &event {
                Event::ImageGeneration(image) => Some(image.id().to_owned()),
                _ => None,
            };
            let is_deferred_phase = matches!(
                self.phase,
                RunPhase::Starting { .. } | RunPhase::Replaying { .. }
            );
            if is_deferred_phase {
                let Some(turn_id) = observed_turn_id else {
                    drop(event);
                    return RunEventOutcome::IgnoredBeforeTurn;
                };
                if let Event::ImageGeneration(image) = &event {
                    let Some(image_bytes) = image.retained_bytes() else {
                        drop(event);
                        return self.record_failure(
                            turn_id.to_owned(),
                            Error::new(
                                ErrorKind::ResourceLimit,
                                "run.images",
                                "pre-acknowledgement image payload overflowed the platform limit",
                            ),
                        );
                    };
                    let previous_bytes = self
                        .deferred_image_bytes_by_id
                        .get(image.id())
                        .copied()
                        .unwrap_or(0);
                    let Some(deferred_bytes) = self
                        .deferred_image_bytes
                        .checked_sub(previous_bytes)
                        .and_then(|bytes| bytes.checked_add(image_bytes))
                    else {
                        drop(event);
                        return self.record_failure(
                            turn_id.to_owned(),
                            Error::new(
                                ErrorKind::ResourceLimit,
                                "run.images",
                                "pre-acknowledgement image payload size overflowed the platform limit",
                            ),
                        );
                    };
                    if deferred_bytes > self.maximum_output_bytes {
                        drop(event);
                        return self.record_failure(
                            turn_id.to_owned(),
                            Error::new(
                                ErrorKind::ResourceLimit,
                                "run.images",
                                format!(
                                    "pre-acknowledgement image payloads exceeded {} bytes; the turn will be interrupted",
                                    self.maximum_output_bytes
                                ),
                            ),
                        );
                    }
                    self.deferred_image_bytes = deferred_bytes;
                    self.deferred_image_bytes_by_id
                        .insert(image.id().to_owned(), image_bytes);
                }
            }
            let deferred = match &mut self.phase {
                RunPhase::Starting { deferred } | RunPhase::Replaying { deferred, .. } => {
                    Some(deferred)
                }
                RunPhase::Active { .. } | RunPhase::TerminalBeforeStart { .. } => None,
            };
            if let Some(deferred) = deferred {
                let Some(turn_id) = observed_turn_id else {
                    drop(event);
                    return RunEventOutcome::IgnoredBeforeTurn;
                };
                if let Some(image_id) = deferred_image_id.as_deref() {
                    deferred.retain(|notification| {
                        !matches!(
                            notification,
                            DeferredRunNotification::Event { event, .. }
                                if matches!(event.as_ref(), Event::ImageGeneration(image) if image.id() == image_id)
                        )
                    });
                }
                if deferred.len() >= event_capacity {
                    drop(event);
                    return self.record_failure(
                        turn_id.to_owned(),
                        Error::new(
                            ErrorKind::ConsumerLagged,
                            "run.events",
                            "bounded pre-acknowledgement event queue is full; the turn will be interrupted",
                        ),
                    );
                }
                deferred.push_back(DeferredRunNotification::Event {
                    turn_id: turn_id.to_owned(),
                    source_method: source_method.to_owned(),
                    event: Box::new(event),
                });
                return RunEventOutcome::Deferred;
            }
        }

        self.route_event_after_start(observed_turn_id, event, event_capacity)
    }

    fn route_event_after_start(
        &mut self,
        observed_turn_id: Option<&str>,
        event: Event,
        event_capacity: usize,
    ) -> RunEventOutcome {
        let mismatch = observed_turn_id.and_then(|observed| {
            self.phase.turn_id().and_then(|expected| {
                (expected != observed).then(|| (expected.to_owned(), observed.to_owned()))
            })
        });
        if let Some((expected, observed)) = mismatch {
            drop(event);
            return RunEventOutcome::TurnMismatch { expected, observed };
        }
        if self.phase.is_terminal() {
            drop(event);
            return RunEventOutcome::AfterTerminal;
        }
        if self.pending_failure.is_some() {
            drop(event);
            return RunEventOutcome::IgnoredAfterFailure;
        }
        if self.abandoned.load(Ordering::Acquire) {
            drop(event);
            return RunEventOutcome::IgnoredAfterAbandon;
        }

        match &event {
            Event::TextDelta(delta) => {
                let Some(output_bytes) = self
                    .text
                    .len()
                    .checked_add(delta.len())
                    .and_then(|bytes| bytes.checked_add(self.image_generation_bytes))
                else {
                    let Some(turn_id) =
                        self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                    else {
                        return RunEventOutcome::AfterTerminal;
                    };
                    return self.record_failure(
                        turn_id,
                        Error::new(
                            ErrorKind::ResourceLimit,
                            "run.output",
                            "assistant output size overflowed the platform limit",
                        ),
                    );
                };
                if output_bytes > self.maximum_output_bytes {
                    let Some(turn_id) =
                        self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                    else {
                        return RunEventOutcome::AfterTerminal;
                    };
                    return self.record_failure(
                        turn_id,
                        Error::new(
                            ErrorKind::ResourceLimit,
                            "run.output",
                            format!(
                                "assistant output exceeded {} bytes; the turn will be interrupted",
                                self.maximum_output_bytes
                            ),
                        ),
                    );
                }
                self.text.push_str(delta);
            }
            Event::UsageUpdated(usage) => self.usage = Some(*usage),
            Event::ImageGeneration(image) => {
                let Some(image_bytes) = image.retained_bytes() else {
                    let Some(turn_id) =
                        self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                    else {
                        return RunEventOutcome::AfterTerminal;
                    };
                    return self.record_failure(
                        turn_id,
                        Error::new(
                            ErrorKind::ResourceLimit,
                            "run.images",
                            "generated image metadata overflowed the platform limit",
                        ),
                    );
                };
                let existing = self
                    .image_generations
                    .iter()
                    .position(|candidate| candidate.id() == image.id());
                if existing.is_none() && self.image_generations.len() >= event_capacity {
                    let Some(turn_id) =
                        self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                    else {
                        return RunEventOutcome::AfterTerminal;
                    };
                    return self.record_failure(
                        turn_id,
                        Error::new(
                            ErrorKind::ResourceLimit,
                            "run.images",
                            "generated image item count exceeded the bounded event capacity",
                        ),
                    );
                }
                let previous_bytes = existing
                    .and_then(|index| self.image_generations[index].retained_bytes())
                    .unwrap_or(0);
                let Some(retained_bytes) = self
                    .image_generation_bytes
                    .checked_sub(previous_bytes)
                    .and_then(|bytes| bytes.checked_add(image_bytes))
                else {
                    let Some(turn_id) =
                        self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                    else {
                        return RunEventOutcome::AfterTerminal;
                    };
                    return self.record_failure(
                        turn_id,
                        Error::new(
                            ErrorKind::ResourceLimit,
                            "run.images",
                            "generated image output size overflowed the platform limit",
                        ),
                    );
                };
                if self
                    .text
                    .len()
                    .checked_add(retained_bytes)
                    .is_none_or(|bytes| bytes > self.maximum_output_bytes)
                {
                    let Some(turn_id) =
                        self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                    else {
                        return RunEventOutcome::AfterTerminal;
                    };
                    return self.record_failure(
                        turn_id,
                        Error::new(
                            ErrorKind::ResourceLimit,
                            "run.images",
                            format!(
                                "generated image output exceeded {} retained bytes; the turn will be interrupted",
                                self.maximum_output_bytes
                            ),
                        ),
                    );
                }
                match existing {
                    Some(index) => self.image_generations[index] = image.clone(),
                    None => self.image_generations.push(image.clone()),
                }
                self.image_generation_bytes = retained_bytes;
            }
            _ => {}
        }
        match self.events.try_send(event) {
            Ok(()) => RunEventOutcome::Delivered,
            Err(TrySendError::Full(_)) => {
                let Some(turn_id) = self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                else {
                    return RunEventOutcome::AfterTerminal;
                };
                self.record_failure(
                    turn_id,
                    Error::new(
                        ErrorKind::ConsumerLagged,
                        "run.events",
                        "bounded run event queue is full; the turn will be interrupted",
                    ),
                )
            }
            Err(TrySendError::Closed(_)) => {
                let Some(turn_id) = self.phase.turn_id().or(observed_turn_id).map(str::to_owned)
                else {
                    return RunEventOutcome::AfterTerminal;
                };
                self.record_failure(
                    turn_id,
                    Error::new(
                        ErrorKind::Disconnected,
                        "run.events",
                        "run event consumer was dropped",
                    ),
                )
            }
        }
    }

    fn record_failure(&mut self, turn_id: String, error: Error) -> RunEventOutcome {
        if self.pending_failure.is_some() {
            return RunEventOutcome::IgnoredAfterFailure;
        }
        match &mut self.phase {
            RunPhase::Starting { deferred } | RunPhase::Replaying { deferred, .. } => {
                deferred.clear();
            }
            RunPhase::Active { .. } | RunPhase::TerminalBeforeStart { .. } => {}
        }
        self.clear_deferred_image_bytes();
        self.pending_failure = Some(error);
        RunEventOutcome::RouteFailure {
            turn_id,
            control: Arc::clone(&self.control),
        }
    }

    fn clear_deferred_image_bytes(&mut self) {
        self.deferred_image_bytes = 0;
        self.deferred_image_bytes_by_id.clear();
    }
}

/// Shared ownership for one provider turn's interrupt and terminal lifecycle.
pub(crate) struct RunControl {
    state: AtomicU8,
    interrupt_result: watch::Sender<Option<Result<()>>>,
    provider_terminal: watch::Sender<bool>,
}

impl RunControl {
    pub(crate) fn new() -> Self {
        let (interrupt_result, _) = watch::channel(None);
        let (provider_terminal, _) = watch::channel(false);
        Self {
            state: AtomicU8::new(0),
            interrupt_result,
            provider_terminal,
        }
    }

    pub(crate) fn interrupt_started(&self) -> bool {
        self.state.load(Ordering::Acquire) & RUN_INTERRUPT_STARTED != 0
    }

    pub(crate) fn provider_terminal_observed(&self) -> bool {
        self.state.load(Ordering::Acquire) & RUN_PROVIDER_TERMINAL != 0
    }

    pub(crate) fn subscribe_interrupt_result(&self) -> watch::Receiver<Option<Result<()>>> {
        self.interrupt_result.subscribe()
    }

    pub(crate) fn subscribe_provider_terminal(&self) -> watch::Receiver<bool> {
        self.provider_terminal.subscribe()
    }

    pub(crate) fn try_start_interrupt(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & (RUN_INTERRUPT_STARTED | RUN_PROVIDER_TERMINAL) != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | RUN_INTERRUPT_STARTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => state = observed,
            }
        }
    }

    pub(crate) fn mark_provider_terminal(&self) {
        self.state.fetch_or(RUN_PROVIDER_TERMINAL, Ordering::AcqRel);
        let _ = self.provider_terminal.send_replace(true);
    }
}

/// Ensures every started interrupt publishes exactly one completion result.
pub(crate) struct InterruptCompletionGuard {
    control: Arc<RunControl>,
    armed: bool,
}

impl InterruptCompletionGuard {
    pub(crate) fn new(control: Arc<RunControl>) -> Self {
        Self {
            control,
            armed: true,
        }
    }

    pub(crate) fn complete(mut self, result: Result<()>) {
        let _ = self.control.interrupt_result.send_replace(Some(result));
        self.armed = false;
    }
}

impl Drop for InterruptCompletionGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .control
                .interrupt_result
                .send_replace(Some(Err(Error::new(
                    ErrorKind::Disconnected,
                    "turn.interrupt",
                    "interrupt request task ended before producing a result",
                ))));
        }
    }
}

pub(crate) struct RunChannels {
    pub(super) events: mpsc::Receiver<Event>,
    pub(super) terminal: watch::Receiver<Option<Result<RunResult>>>,
    pub(super) active: Arc<AtomicBool>,
    pub(super) abandoned: Arc<AtomicBool>,
    pub(super) control: Arc<RunControl>,
}

impl RunRegistry {
    pub(crate) fn new() -> Self {
        Self {
            routes: Mutex::new(HashMap::new()),
        }
    }

    fn routes(&self) -> MutexGuard<'_, HashMap<String, RunRoute>> {
        // The route aggregate performs only bounded in-memory transitions and
        // non-blocking channel sends. It never holds this guard across I/O or
        // an await point.
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn contains(&self, thread_id: &str) -> bool {
        self.routes().contains_key(thread_id)
    }

    pub(crate) fn active_turn(&self, thread_id: &str) -> Option<(String, Arc<RunControl>)> {
        self.routes().get(thread_id).and_then(|route| {
            route
                .phase
                .turn_id()
                .map(|turn_id| (turn_id.to_owned(), Arc::clone(&route.control)))
        })
    }

    pub(crate) fn tracks_abandoned_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        control: &Arc<RunControl>,
    ) -> bool {
        self.routes().get(thread_id).is_some_and(|route| {
            Arc::ptr_eq(&route.control, control)
                && route.abandoned.load(Ordering::Acquire)
                && route
                    .phase
                    .turn_id()
                    .is_none_or(|observed| observed == turn_id)
        })
    }

    pub(crate) fn fail_before_start(
        &self,
        thread_id: &str,
        error: Error,
    ) -> PreStartFailureTransition {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(thread_id) else {
            return PreStartFailureTransition::Ignored;
        };
        match route.phase {
            RunPhase::Starting { .. } => {
                route.control.mark_provider_terminal();
                route.active.store(false, Ordering::Release);
                let _ = route.terminal.send(Some(Err(error)));
                route.phase = RunPhase::TerminalBeforeStart { turn_id: None };
                PreStartFailureTransition::Applied
            }
            RunPhase::TerminalBeforeStart { .. } => PreStartFailureTransition::Ignored,
            RunPhase::Replaying { .. } | RunPhase::Active { .. } => {
                PreStartFailureTransition::ProviderTurnOwned
            }
        }
    }

    pub(crate) fn cancel_registered(&self, thread_id: &str) -> Option<(String, Arc<RunControl>)> {
        let mut routes = self.routes();
        let route = routes.get_mut(thread_id)?;
        if route.phase.is_terminal() {
            route.control.mark_provider_terminal();
            route.active.store(false, Ordering::Release);
            routes.remove(thread_id);
            return None;
        }
        route.abandoned.store(true, Ordering::Release);
        match route.phase.turn_id().map(str::to_owned) {
            Some(turn_id) => Some((turn_id, Arc::clone(&route.control))),
            None => {
                route.control.mark_provider_terminal();
                route.active.store(false, Ordering::Release);
                routes.remove(thread_id);
                None
            }
        }
    }

    pub(crate) fn acknowledge_start(&self, thread_id: &str, turn_id: &str) -> StartTurnTransition {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(thread_id) else {
            return StartTurnTransition::MissingRoute;
        };
        let transition = match &mut route.phase {
            RunPhase::Starting { deferred } => {
                let deferred = std::mem::take(deferred);
                route.clear_deferred_image_bytes();
                route.phase = RunPhase::Replaying {
                    turn_id: turn_id.to_owned(),
                    deferred: VecDeque::new(),
                };
                StartTurnTransition::Replay(deferred)
            }
            RunPhase::TerminalBeforeStart {
                turn_id: terminal_turn_id,
            } => {
                if let Some(expected) = terminal_turn_id.as_deref()
                    && expected != turn_id
                {
                    return StartTurnTransition::TerminalTurnMismatch {
                        expected: expected.to_owned(),
                    };
                }
                StartTurnTransition::CompletedBeforeAcknowledgement
            }
            RunPhase::Replaying { .. } | RunPhase::Active { .. } => {
                StartTurnTransition::AlreadyAcknowledged
            }
        };
        if matches!(
            transition,
            StartTurnTransition::CompletedBeforeAcknowledgement
        ) {
            routes.remove(thread_id);
        }
        transition
    }

    pub(crate) fn replay_transition(&self, thread_id: &str, turn_id: &str) -> ReplayTransition {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(thread_id) else {
            return ReplayTransition::Stopped;
        };
        match &mut route.phase {
            RunPhase::Replaying {
                turn_id: active_turn,
                deferred,
            } if active_turn == turn_id => {
                if deferred.is_empty() {
                    route.phase = RunPhase::Active {
                        turn_id: turn_id.to_owned(),
                    };
                    ReplayTransition::Active
                } else {
                    let deferred = std::mem::take(deferred);
                    route.clear_deferred_image_bytes();
                    ReplayTransition::Next(deferred)
                }
            }
            RunPhase::Starting { .. }
            | RunPhase::Replaying { .. }
            | RunPhase::Active { .. }
            | RunPhase::TerminalBeforeStart { .. } => ReplayTransition::Stopped,
        }
    }

    pub(crate) fn route_event(
        &self,
        thread_id: &str,
        observed_turn_id: Option<&str>,
        source_method: &str,
        event: Event,
        defer_while_starting: bool,
        event_capacity: usize,
    ) -> RunEventOutcome {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(thread_id) else {
            drop(event);
            return RunEventOutcome::Unregistered;
        };
        route.route_event(
            observed_turn_id,
            source_method,
            event,
            defer_while_starting,
            event_capacity,
        )
    }

    pub(crate) fn route_terminal(
        &self,
        thread_id: &str,
        completion: TurnCompletion,
        params: &Value,
        defer_while_starting: bool,
        event_capacity: usize,
    ) -> TerminalRouteOutcome {
        let turn_id = completion.turn_id().to_owned();
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(thread_id) else {
            return TerminalRouteOutcome::Unregistered;
        };

        let mut queue_failure = None;
        if defer_while_starting && route.pending_failure.is_none() {
            let deferred = match &mut route.phase {
                RunPhase::Starting { deferred } | RunPhase::Replaying { deferred, .. } => {
                    Some(deferred)
                }
                RunPhase::Active { .. } | RunPhase::TerminalBeforeStart { .. } => None,
            };
            if let Some(deferred) = deferred {
                if deferred.len() < event_capacity {
                    deferred.push_back(DeferredRunNotification::Terminal(params.clone()));
                    return TerminalRouteOutcome::Deferred;
                }
                deferred.clear();
                queue_failure = Some(Error::new(
                    ErrorKind::ConsumerLagged,
                    "run.events",
                    "bounded pre-acknowledgement event queue was full when the provider terminal state arrived",
                ));
            }
        }

        if route.phase.is_terminal() {
            return TerminalRouteOutcome::Duplicate { turn_id };
        }
        if let Some(expected) = route.phase.turn_id()
            && expected != turn_id
        {
            return TerminalRouteOutcome::TurnMismatch {
                expected: expected.to_owned(),
                observed: turn_id,
            };
        }

        let result = match route.pending_failure.take().or(queue_failure) {
            Some(error) => Err(error),
            None => completion.into_result(
                thread_id,
                route.text.clone(),
                route.usage,
                route.image_generations.clone(),
            ),
        };
        route.control.mark_provider_terminal();
        route.active.store(false, Ordering::Release);
        let _ = route.terminal.send(Some(result));

        let remove_route = matches!(
            route.phase,
            RunPhase::Replaying { .. } | RunPhase::Active { .. }
        );
        if matches!(route.phase, RunPhase::Starting { .. }) {
            route.phase = RunPhase::TerminalBeforeStart {
                turn_id: Some(turn_id),
            };
        }
        if remove_route {
            routes.remove(thread_id);
        }
        TerminalRouteOutcome::Completed
    }

    pub(crate) fn fail_active(
        &self,
        thread_id: &str,
        turn_id: &str,
        control: &Arc<RunControl>,
        error: Error,
    ) -> bool {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(thread_id) else {
            return false;
        };
        if route.phase.is_terminal()
            || route.phase.turn_id() != Some(turn_id)
            || !Arc::ptr_eq(&route.control, control)
            || route.pending_failure.is_some()
        {
            return false;
        }
        route.pending_failure = Some(error);
        true
    }

    pub(crate) fn fail_all(&self, error: &Error) {
        let routes = std::mem::take(&mut *self.routes());
        for (_, mut route) in routes {
            route.control.mark_provider_terminal();
            route.active.store(false, Ordering::Release);
            if route.terminal.borrow().is_none() {
                let terminal_error = route.pending_failure.take().map_or_else(
                    || error.clone(),
                    |primary| {
                        primary.with_related_error("provider turn cleanup also failed", error)
                    },
                );
                let _ = route.terminal.send(Some(Err(terminal_error)));
            }
        }
    }

    pub(crate) fn register(
        &self,
        thread_id: &str,
        active: Arc<AtomicBool>,
        event_capacity: usize,
        maximum_output_bytes: usize,
    ) -> Result<RunChannels> {
        let (event_tx, event_rx) = mpsc::channel(event_capacity);
        let (terminal_tx, terminal_rx) = watch::channel(None);
        let abandoned = Arc::new(AtomicBool::new(false));
        let control = Arc::new(RunControl::new());
        let mut routes = self.routes();
        if routes.contains_key(thread_id) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "turn.start",
                "session already has an active run",
            ));
        }
        routes.insert(
            thread_id.to_owned(),
            RunRoute {
                events: event_tx,
                terminal: terminal_tx,
                active: Arc::clone(&active),
                abandoned: Arc::clone(&abandoned),
                control: Arc::clone(&control),
                phase: RunPhase::Starting {
                    deferred: VecDeque::new(),
                },
                pending_failure: None,
                text: String::new(),
                maximum_output_bytes,
                usage: None,
                image_generations: Vec::new(),
                image_generation_bytes: 0,
                deferred_image_bytes: 0,
                deferred_image_bytes_by_id: HashMap::new(),
            },
        );
        Ok(RunChannels {
            events: event_rx,
            terminal: terminal_rx,
            active,
            abandoned,
            control,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeferredRunNotification, PreStartFailureTransition, ReplayTransition, RunEventOutcome,
        RunPhase, RunRegistry, StartTurnTransition, TerminalRouteOutcome,
    };
    use crate::error::{Error, ErrorKind};
    use crate::event::{Event, TurnCompletion};
    use crate::image::ImageGeneration;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn active_thread_has_only_one_run_route() {
        let registry = RunRegistry::new();
        let active = Arc::new(AtomicBool::new(true));
        let _first = registry
            .register("thread-1", Arc::clone(&active), 4, 1_024)
            .expect("first run route should register");
        let error = registry
            .register("thread-1", active, 4, 1_024)
            .err()
            .expect("duplicate run route should fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn starting_route_defers_events_without_delivering_them() {
        let registry = RunRegistry::new();
        let mut channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");

        let outcome = {
            let mut routes = registry.routes();
            routes.get_mut("thread-1").expect("route").route_event(
                Some("turn-1"),
                "turn/started",
                Event::Started,
                true,
                4,
            )
        };

        assert!(matches!(outcome, RunEventOutcome::Deferred));
        assert!(channels.events.try_recv().is_err());
        let routes = registry.routes();
        assert!(matches!(
            &routes.get("thread-1").expect("route").phase,
            RunPhase::Starting { deferred } if deferred.len() == 1
        ));
    }

    #[test]
    fn pre_acknowledgement_images_count_latest_state_per_item() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 32)
            .expect("run route");
        let mut routes = registry.routes();
        let route = routes.get_mut("thread-1").expect("route");

        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/started",
                Event::ImageGeneration(image("inProgress", "")),
                true,
                4,
            ),
            RunEventOutcome::Deferred
        ));
        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/completed",
                Event::ImageGeneration(image("completed", "iVBORw0KGgo=")),
                true,
                4,
            ),
            RunEventOutcome::Deferred
        ));
        assert_eq!(route.deferred_image_bytes, 28);
        assert_eq!(route.deferred_image_bytes_by_id.len(), 1);
        assert_eq!(route.deferred_image_bytes_by_id.get("image-1"), Some(&28));
        assert!(matches!(
            &route.phase,
            RunPhase::Starting { deferred }
                if matches!(deferred.front(), Some(DeferredRunNotification::Event { event, .. })
                    if matches!(event.as_ref(), Event::ImageGeneration(image)
                        if image.status() == "completed" && image.result_base64() == "iVBORw0KGgo="))
                    && deferred.len() == 1
        ));
        assert!(route.pending_failure.is_none());
    }

    #[test]
    fn pre_acknowledgement_distinct_images_remain_bounded() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 32)
            .expect("run route");
        let mut routes = registry.routes();
        let route = routes.get_mut("thread-1").expect("route");

        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/completed",
                Event::ImageGeneration(image("completed", "iVBORw0KGgo=")),
                true,
                4,
            ),
            RunEventOutcome::Deferred
        ));
        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/completed",
                Event::ImageGeneration(image_with_id("image-2", "completed", "iVBORw0KGgo=")),
                true,
                4,
            ),
            RunEventOutcome::RouteFailure { .. }
        ));
        assert_eq!(route.deferred_image_bytes, 0);
        assert!(route.deferred_image_bytes_by_id.is_empty());
        assert!(matches!(
            &route.phase,
            RunPhase::Starting { deferred } if deferred.is_empty()
        ));
        assert_eq!(
            route.pending_failure.as_ref().map(Error::kind),
            Some(ErrorKind::ResourceLimit)
        );
    }

    #[test]
    fn full_event_queue_records_one_terminal_failure() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 1, 1_024)
            .expect("run route");
        let mut routes = registry.routes();
        let route = routes.get_mut("thread-1").expect("route");
        route.phase = RunPhase::Active {
            turn_id: "turn-1".to_owned(),
        };

        assert!(matches!(
            route.route_event(Some("turn-1"), "turn/started", Event::Started, false, 1),
            RunEventOutcome::Delivered
        ));
        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/agentMessage/delta",
                Event::TextDelta("lost".to_owned()),
                false,
                1,
            ),
            RunEventOutcome::RouteFailure { .. }
        ));
        assert_eq!(
            route.pending_failure.as_ref().map(|error| error.kind()),
            Some(ErrorKind::ConsumerLagged)
        );
        assert!(matches!(
            route.route_event(Some("turn-1"), "turn/started", Event::Started, false, 1),
            RunEventOutcome::IgnoredAfterFailure
        ));
    }

    #[test]
    fn cumulative_output_limit_interrupts_before_unbounded_growth() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 4)
            .expect("run route");
        let mut routes = registry.routes();
        let route = routes.get_mut("thread-1").expect("route");
        route.phase = RunPhase::Active {
            turn_id: "turn-1".to_owned(),
        };

        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/agentMessage/delta",
                Event::TextDelta("1234".to_owned()),
                false,
                4,
            ),
            RunEventOutcome::Delivered
        ));
        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/agentMessage/delta",
                Event::TextDelta("5".to_owned()),
                false,
                4,
            ),
            RunEventOutcome::RouteFailure { .. }
        ));
        assert_eq!(route.text, "1234");
        assert_eq!(
            route.pending_failure.as_ref().map(Error::kind),
            Some(ErrorKind::ResourceLimit)
        );
    }

    #[test]
    fn image_lifecycle_is_replaced_and_retained_in_terminal_result() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            let route = routes.get_mut("thread-1").expect("route");
            route.phase = RunPhase::Active {
                turn_id: "turn-1".to_owned(),
            };
            assert!(matches!(
                route.route_event(
                    Some("turn-1"),
                    "item/started",
                    Event::ImageGeneration(image("inProgress", "")),
                    false,
                    4,
                ),
                RunEventOutcome::Delivered
            ));
            assert!(matches!(
                route.route_event(
                    Some("turn-1"),
                    "item/completed",
                    Event::ImageGeneration(image("completed", "iVBORw0KGgo=")),
                    false,
                    4,
                ),
                RunEventOutcome::Delivered
            ));
            assert_eq!(route.image_generations.len(), 1);
            assert_eq!(route.image_generations[0].status(), "completed");
        }

        assert!(matches!(
            registry.route_terminal(
                "thread-1",
                TurnCompletion::completed("turn-1".to_owned()),
                &json!({}),
                false,
                4,
            ),
            TerminalRouteOutcome::Completed
        ));
        let result = channels
            .terminal
            .borrow()
            .clone()
            .expect("terminal result")
            .expect("successful terminal result");
        assert_eq!(
            result.image_generations,
            vec![image("completed", "iVBORw0KGgo=")]
        );
    }

    #[test]
    fn image_output_limit_fails_before_retaining_unbounded_bytes() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 16)
            .expect("run route");
        let mut routes = registry.routes();
        let route = routes.get_mut("thread-1").expect("route");
        route.phase = RunPhase::Active {
            turn_id: "turn-1".to_owned(),
        };
        assert!(matches!(
            route.route_event(
                Some("turn-1"),
                "item/completed",
                Event::ImageGeneration(image("completed", "iVBORw0KGgo=")),
                false,
                4,
            ),
            RunEventOutcome::RouteFailure { .. }
        ));
        assert!(route.image_generations.is_empty());
        assert_eq!(
            route.pending_failure.as_ref().map(Error::kind),
            Some(ErrorKind::ResourceLimit)
        );
    }

    fn image(status: &str, result_base64: &str) -> ImageGeneration {
        image_with_id("image-1", status, result_base64)
    }

    fn image_with_id(id: &str, status: &str, result_base64: &str) -> ImageGeneration {
        ImageGeneration::new(
            id.to_owned(),
            status.to_owned(),
            None,
            result_base64.to_owned(),
            Some(false),
            None,
            None,
        )
    }

    #[test]
    fn stale_turn_event_does_not_mutate_accumulated_output() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        let mut routes = registry.routes();
        let route = routes.get_mut("thread-1").expect("route");
        route.phase = RunPhase::Active {
            turn_id: "turn-1".to_owned(),
        };

        assert!(matches!(
            route.route_event(
                Some("turn-2"),
                "item/agentMessage/delta",
                Event::TextDelta("stale".to_owned()),
                false,
                4,
            ),
            RunEventOutcome::TurnMismatch { .. }
        ));
        assert!(route.text.is_empty());
    }

    #[test]
    fn start_acknowledgement_commits_replay_before_active_state() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            let route = routes.get_mut("thread-1").expect("route");
            assert!(matches!(
                route.route_event(Some("turn-1"), "turn/started", Event::Started, true, 4,),
                RunEventOutcome::Deferred
            ));
        }

        let deferred = match registry.acknowledge_start("thread-1", "turn-1") {
            StartTurnTransition::Replay(deferred) => deferred,
            _ => panic!("starting route did not enter replay"),
        };
        assert_eq!(deferred.len(), 1);
        assert!(matches!(
            registry.replay_transition("thread-1", "turn-1"),
            ReplayTransition::Active
        ));
        let routes = registry.routes();
        assert!(matches!(
            &routes.get("thread-1").expect("route").phase,
            RunPhase::Active { turn_id } if turn_id == "turn-1"
        ));
    }

    #[test]
    fn terminal_before_acknowledgement_requires_the_same_turn() {
        let registry = RunRegistry::new();
        let _channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            routes.get_mut("thread-1").expect("route").phase = RunPhase::TerminalBeforeStart {
                turn_id: Some("turn-1".to_owned()),
            };
        }

        assert!(matches!(
            registry.acknowledge_start("thread-1", "turn-2"),
            StartTurnTransition::TerminalTurnMismatch { expected } if expected == "turn-1"
        ));
        assert!(registry.routes().contains_key("thread-1"));
        assert!(matches!(
            registry.acknowledge_start("thread-1", "turn-1"),
            StartTurnTransition::CompletedBeforeAcknowledgement
        ));
        assert!(!registry.routes().contains_key("thread-1"));
    }

    #[test]
    fn pre_start_failure_closes_only_an_unowned_route() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        let failure =
            crate::error::Error::new(ErrorKind::InvalidInput, "turn.start", "invalid prompt");

        assert!(matches!(
            registry.fail_before_start("thread-1", failure.clone()),
            PreStartFailureTransition::Applied
        ));
        assert!(!channels.active.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(channels.terminal.borrow().clone(), Some(Err(failure)));
        let routes = registry.routes();
        assert!(matches!(
            &routes.get("thread-1").expect("route").phase,
            RunPhase::TerminalBeforeStart { turn_id: None }
        ));
    }

    #[test]
    fn cancelling_an_active_route_preserves_remote_turn_ownership() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            routes.get_mut("thread-1").expect("route").phase = RunPhase::Active {
                turn_id: "turn-1".to_owned(),
            };
        }

        let (turn_id, control) = registry
            .cancel_registered("thread-1")
            .expect("active remote turn must remain tracked for interruption");
        assert_eq!(turn_id, "turn-1");
        assert!(registry.contains("thread-1"));
        assert!(registry.tracks_abandoned_turn("thread-1", "turn-1", &control));
        assert!(channels.active.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn cancelling_before_remote_ownership_removes_the_route() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");

        assert!(registry.cancel_registered("thread-1").is_none());
        assert!(!registry.contains("thread-1"));
        assert!(!channels.active.load(std::sync::atomic::Ordering::Acquire));
        assert!(channels.control.provider_terminal_observed());
    }

    #[test]
    fn active_terminal_completion_publishes_result_and_removes_route() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            let route = routes.get_mut("thread-1").expect("route");
            route.phase = RunPhase::Active {
                turn_id: "turn-1".to_owned(),
            };
            route.text = "done".to_owned();
        }

        assert!(matches!(
            registry.route_terminal(
                "thread-1",
                TurnCompletion::completed("turn-1".to_owned()),
                &json!({}),
                false,
                4,
            ),
            TerminalRouteOutcome::Completed
        ));
        assert!(!registry.contains("thread-1"));
        assert!(!channels.active.load(std::sync::atomic::Ordering::Acquire));
        let result = channels
            .terminal
            .borrow()
            .clone()
            .expect("terminal result")
            .expect("successful result");
        assert_eq!(result.turn_id, "turn-1");
        assert_eq!(result.text, "done");
        assert!(channels.control.provider_terminal_observed());
    }

    #[test]
    fn terminal_before_start_acknowledgement_is_deferred_then_retained() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        let params = json!({
            "threadId": "thread-1",
            "turn": {"id": "turn-1", "status": "completed"}
        });

        assert!(matches!(
            registry.route_terminal(
                "thread-1",
                TurnCompletion::completed("turn-1".to_owned()),
                &params,
                true,
                4,
            ),
            TerminalRouteOutcome::Deferred
        ));
        assert!(channels.terminal.borrow().is_none());
        assert!(matches!(
            registry.acknowledge_start("thread-1", "turn-1"),
            StartTurnTransition::Replay(deferred) if deferred.len() == 1
        ));
        assert!(matches!(
            registry.route_terminal(
                "thread-1",
                TurnCompletion::completed("turn-1".to_owned()),
                &params,
                false,
                4,
            ),
            TerminalRouteOutcome::Completed
        ));
        assert!(!registry.contains("thread-1"));
        assert!(channels.terminal.borrow().is_some());
    }

    #[test]
    fn disconnect_cleanup_fails_every_route_without_hiding_prior_failure() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            let route = routes.get_mut("thread-1").expect("route");
            route.phase = RunPhase::Active {
                turn_id: "turn-1".to_owned(),
            };
            route.pending_failure = Some(Error::new(
                ErrorKind::ConsumerLagged,
                "run.events",
                "consumer stopped draining events",
            ));
        }

        let disconnect = Error::new(
            ErrorKind::Disconnected,
            "process.read",
            "provider transport closed",
        );
        registry.fail_all(&disconnect);

        assert!(!registry.contains("thread-1"));
        assert!(!channels.active.load(Ordering::Acquire));
        assert!(channels.control.provider_terminal_observed());
        let terminal = channels
            .terminal
            .borrow()
            .clone()
            .expect("terminal result")
            .expect_err("disconnect must fail the run");
        assert_eq!(terminal.kind(), ErrorKind::ConsumerLagged);
        assert_eq!(terminal.operation(), "run.events");
        assert!(
            terminal
                .message()
                .contains("consumer stopped draining events")
        );
        assert!(
            terminal
                .message()
                .contains("provider turn cleanup also failed")
        );
        assert!(terminal.message().contains("process.read"));
    }

    #[test]
    fn active_deadline_failure_wins_over_provider_terminal_status() {
        let registry = RunRegistry::new();
        let channels = registry
            .register("thread-1", Arc::new(AtomicBool::new(true)), 4, 1_024)
            .expect("run route");
        {
            let mut routes = registry.routes();
            routes.get_mut("thread-1").expect("route").phase = RunPhase::Active {
                turn_id: "turn-1".to_owned(),
            };
        }

        let timeout = Error::new(ErrorKind::Timeout, "turn.run", "deadline");
        assert!(registry.fail_active("thread-1", "turn-1", &channels.control, timeout.clone()));
        assert!(!registry.fail_active("thread-1", "turn-1", &channels.control, timeout));
        assert!(matches!(
            registry.route_terminal(
                "thread-1",
                TurnCompletion::interrupted("turn-1".to_owned()),
                &json!({}),
                false,
                4,
            ),
            TerminalRouteOutcome::Completed
        ));
        let terminal = channels
            .terminal
            .borrow()
            .clone()
            .expect("terminal result")
            .expect_err("deadline must remain primary");
        assert_eq!(terminal.kind(), ErrorKind::Timeout);
        assert_eq!(terminal.operation(), "turn.run");
    }
}
