//! Codex sessions and runs.

mod run_state;

pub(crate) use run_state::{
    DeferredRunNotification, InterruptCompletionGuard, PreStartFailureTransition, ReplayTransition,
    RunChannels, RunControl, RunEventOutcome, RunRegistry, StartTurnTransition,
    TerminalRouteOutcome,
};

use crate::client::ClientInner;
use crate::error::{Error, ErrorKind, Result};
use crate::event::{Event, RunResult, TurnAudit};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard, mpsc, watch};

/// In-process ownership of loaded thread identifiers and their transition lock.
pub(crate) struct SessionRegistry {
    ids: StdMutex<HashSet<String>>,
    lifecycle: Mutex<()>,
}

impl SessionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            ids: StdMutex::new(HashSet::new()),
            lifecycle: Mutex::new(()),
        }
    }

    fn ids(&self) -> StdMutexGuard<'_, HashSet<String>> {
        // The collection lock guards only short in-memory ownership changes and
        // never spans an await. Poison recovery avoids leaking a remote thread.
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) async fn lock_lifecycle(&self) -> MutexGuard<'_, ()> {
        self.lifecycle.lock().await
    }

    pub(crate) fn contains(&self, thread_id: &str) -> bool {
        self.ids().contains(thread_id)
    }

    pub(crate) fn insert(&self, thread_id: String) -> bool {
        self.ids().insert(thread_id)
    }

    pub(crate) fn remove(&self, thread_id: &str) -> bool {
        self.ids().remove(thread_id)
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.ids().iter().cloned().collect()
    }

    pub(crate) fn clear(&self) {
        self.ids().clear();
    }
}

/// Filesystem sandbox exposed by Vergerail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sandbox {
    /// The turn cannot write files and has no network access.
    ReadOnly,
    /// The turn may write only below the exact session working directory and has
    /// no network access.
    WorkspaceWrite,
}

impl Sandbox {
    pub(crate) const fn mode(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }

    pub(crate) const fn approval_policy(self) -> &'static str {
        match self {
            Self::ReadOnly => "never",
            Self::WorkspaceWrite => "on-request",
        }
    }
}

/// Reasoning effort sent to the pinned Codex turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReasoningEffort {
    /// Disable explicit reasoning effort.
    None,
    /// Use low reasoning effort.
    Low,
    /// Use the balanced default reasoning effort.
    #[default]
    Medium,
    /// Use high reasoning effort.
    High,
    /// Use extra-high reasoning effort.
    XHigh,
    /// Use the maximum reasoning effort supported by the selected model.
    Max,
}

impl ReasoningEffort {
    pub(crate) const fn value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Explicit configuration for a Codex session.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    cwd: PathBuf,
    sandbox: Sandbox,
    ephemeral: bool,
    model: Option<String>,
    reasoning: ReasoningEffort,
    base_instructions: Option<String>,
    developer_instructions: Option<String>,
    text_only: bool,
    image_only: bool,
    output_schema: Option<Value>,
    turn_timeout: Duration,
    maximum_output_bytes: usize,
}

const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MAXIMUM_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAXIMUM_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

impl SessionOptions {
    /// Creates a persistent read-only session rooted at `cwd`.
    #[must_use]
    pub fn read_only(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            sandbox: Sandbox::ReadOnly,
            ephemeral: false,
            model: None,
            reasoning: ReasoningEffort::Medium,
            base_instructions: None,
            developer_instructions: None,
            text_only: false,
            image_only: false,
            output_schema: None,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            maximum_output_bytes: DEFAULT_MAXIMUM_OUTPUT_BYTES,
        }
    }

    /// Creates a persistent session that may write only below `cwd`.
    ///
    /// This mode is deliberately unavailable through [`crate::Codex::run`]. The
    /// caller must consume run events and resolve any approval requests.
    #[must_use]
    pub fn workspace_write(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            sandbox: Sandbox::WorkspaceWrite,
            ephemeral: false,
            model: None,
            reasoning: ReasoningEffort::Medium,
            base_instructions: None,
            developer_instructions: None,
            text_only: false,
            image_only: false,
            output_schema: None,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            maximum_output_bytes: DEFAULT_MAXIMUM_OUTPUT_BYTES,
        }
    }

    /// Prevents Codex from persisting this session to disk.
    #[must_use]
    pub const fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Selects one model name returned by [`crate::Codex::models`].
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Selects the reasoning effort for every turn in this session.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: ReasoningEffort) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Sets the base instructions sent through the dedicated app-server field.
    #[must_use]
    pub fn with_base_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.base_instructions = Some(instructions.into());
        self
    }

    /// Sets developer instructions without flattening them into user input.
    #[must_use]
    pub fn with_developer_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.developer_instructions = Some(instructions.into());
        self
    }

    /// Disables Codex execution, app, web, plugin, memory, hook, and subagent tools.
    ///
    /// The resulting session still emits a durable audit so callers can verify
    /// that no effect-bearing item was persisted.
    #[must_use]
    pub const fn text_only(mut self) -> Self {
        self.text_only = true;
        self
    }

    /// Restricts the app-server thread to exactly one image-generation
    /// capability. Filesystem, network, shell, app, memory, plugin, and
    /// multi-agent surfaces remain disabled.
    #[must_use]
    pub const fn image_only(mut self) -> Self {
        self.image_only = true;
        self
    }

    /// Requires the app-server to return a structured JSON object matching the
    /// supplied output schema. The schema is sent as the native `turn/start`
    /// outputSchema field; callers must still validate the returned bytes.
    #[must_use]
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Sets the maximum lifetime of each provider turn in this session.
    #[must_use]
    pub const fn with_turn_timeout(mut self, timeout: Duration) -> Self {
        self.turn_timeout = timeout;
        self
    }

    /// Sets the maximum cumulative assistant text and image payload retained
    /// for each turn.
    #[must_use]
    pub const fn with_maximum_output_bytes(mut self, bytes: usize) -> Self {
        self.maximum_output_bytes = bytes;
        self
    }

    /// Returns the requested working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Returns the requested sandbox.
    #[must_use]
    pub const fn sandbox(&self) -> Sandbox {
        self.sandbox
    }

    pub(crate) fn validate(self) -> Result<Self> {
        if self.cwd.as_os_str().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "SessionOptions::validate",
                "working directory must be non-empty",
            ));
        }
        if self.text_only && self.image_only {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "SessionOptions::validate",
                "text-only and image-only modes are mutually exclusive",
            ));
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "SessionOptions::validate",
                "model must be non-empty when supplied",
            ));
        }
        for (name, instructions) in [
            ("base instructions", self.base_instructions.as_deref()),
            (
                "developer instructions",
                self.developer_instructions.as_deref(),
            ),
        ] {
            if let Some(instructions) = instructions
                && (instructions.trim().is_empty() || instructions.len() > 1024 * 1024)
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "SessionOptions::validate",
                    format!("{name} must contain 1..=1048576 bytes when supplied"),
                ));
            }
        }
        if self.turn_timeout.is_zero() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "SessionOptions::validate",
                "turn timeout must be greater than zero",
            ));
        }
        if let Some(schema) = self.output_schema.as_ref() {
            if !schema.is_object() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "SessionOptions::validate",
                    "output schema must be a JSON object",
                ));
            }
            let bytes = serde_json::to_vec(schema).map_err(|error| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "SessionOptions::validate",
                    format!("output schema is not serializable: {error}"),
                )
            })?;
            if bytes.len() > 64 * 1024 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "SessionOptions::validate",
                    "output schema exceeds the 64 KiB bound",
                ));
            }
        }
        if !(1..=MAXIMUM_OUTPUT_BYTES).contains(&self.maximum_output_bytes) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "SessionOptions::validate",
                format!("maximum output must contain 1..={MAXIMUM_OUTPUT_BYTES} bytes"),
            ));
        }
        Ok(self)
    }

    pub(crate) const fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    pub(crate) fn model_value(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(crate) const fn reasoning(&self) -> ReasoningEffort {
        self.reasoning
    }

    pub(crate) fn base_instructions(&self) -> Option<&str> {
        self.base_instructions.as_deref()
    }

    pub(crate) fn developer_instructions(&self) -> Option<&str> {
        self.developer_instructions.as_deref()
    }

    pub(crate) const fn is_text_only(&self) -> bool {
        self.text_only
    }

    pub(crate) const fn is_image_only(&self) -> bool {
        self.image_only
    }

    pub(crate) fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }

    pub(crate) const fn turn_timeout(&self) -> Duration {
        self.turn_timeout
    }

    pub(crate) const fn maximum_output_bytes(&self) -> usize {
        self.maximum_output_bytes
    }
}

struct RegisteredRunGuard {
    inner: Arc<ClientInner>,
    thread_id: String,
    armed: bool,
}

impl RegisteredRunGuard {
    fn new(inner: Arc<ClientInner>, thread_id: String) -> Self {
        Self {
            inner,
            thread_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RegisteredRunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let inner = Arc::clone(&self.inner);
            let thread_id = self.thread_id.clone();
            handle.spawn(async move {
                inner.cancel_registered_run(thread_id).await;
            });
        } else {
            self.inner.begin_force_shutdown();
        }
    }
}

/// One Codex thread with at most one active turn.
///
/// Call [`Session::close`] when the thread is no longer needed. Dropping this
/// handle alone does not synchronously unsubscribe it; [`Codex::shutdown`](crate::Codex::shutdown)
/// performs final subscription cleanup.
pub struct Session {
    inner: Arc<ClientInner>,
    thread_id: String,
    cwd: PathBuf,
    sandbox: Sandbox,
    reasoning: ReasoningEffort,
    ephemeral: bool,
    turn_timeout: Duration,
    maximum_output_bytes: usize,
    output_schema: Option<Value>,
    active: Arc<AtomicBool>,
    lifecycle: Mutex<SessionLifecycle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionLifecycle {
    Open,
    Closed,
}

impl Session {
    pub(crate) fn new(
        inner: Arc<ClientInner>,
        thread_id: String,
        cwd: PathBuf,
        options: &SessionOptions,
    ) -> Self {
        Self {
            inner,
            thread_id,
            cwd,
            sandbox: options.sandbox(),
            reasoning: options.reasoning(),
            ephemeral: options.is_ephemeral(),
            turn_timeout: options.turn_timeout(),
            maximum_output_bytes: options.maximum_output_bytes(),
            output_schema: options.output_schema(),
            active: Arc::new(AtomicBool::new(false)),
            lifecycle: Mutex::new(SessionLifecycle::Open),
        }
    }

    /// Returns the Codex thread identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.thread_id
    }

    /// Reads persisted command and file-change evidence for one completed turn.
    ///
    /// Auditing is available only while a persistent session is open and has no
    /// active run.
    pub async fn audit_turn(&self, turn_id: &str) -> Result<TurnAudit> {
        if turn_id.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::audit_turn",
                "turn id must be non-empty",
            ));
        }
        let lifecycle = self.lifecycle.lock().await;
        if *lifecycle != SessionLifecycle::Open {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::audit_turn",
                "session is closed",
            ));
        }
        if self.ephemeral {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::audit_turn",
                "ephemeral sessions do not have persisted turn history",
            ));
        }
        if self.active.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::audit_turn",
                "cannot audit a session with an active run",
            ));
        }
        let _client_lifecycle = self.inner.lock_session_lifecycle().await;
        self.inner.audit_turn(&self.thread_id, turn_id).await
    }

    /// Starts one turn. A session rejects concurrent turns.
    pub async fn start(&self, prompt: impl Into<String>) -> Result<Run> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::start",
                "prompt must be non-empty",
            ));
        }
        let lifecycle = self.lifecycle.lock().await;
        if *lifecycle != SessionLifecycle::Open {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::start",
                "session is closed",
            ));
        }
        // Serialize the non-idempotent turn/start ownership handshake with
        // create, resume, close, and shutdown. The per-session lock is acquired
        // first everywhere to keep the lock order consistent with close().
        let _client_lifecycle = self.inner.lock_session_lifecycle().await;
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::start",
                "session already has an active run",
            ));
        }
        let channels = match self.inner.register_run(
            &self.thread_id,
            Arc::clone(&self.active),
            self.maximum_output_bytes,
        ) {
            Ok(channels) => channels,
            Err(error) => {
                self.active.store(false, Ordering::Release);
                return Err(error);
            }
        };
        let mut registration =
            RegisteredRunGuard::new(Arc::clone(&self.inner), self.thread_id.clone());
        let turn_id = self
            .inner
            .start_turn(
                &self.thread_id,
                &self.cwd,
                self.sandbox,
                self.reasoning,
                prompt,
                self.output_schema.clone(),
            )
            .await?;
        registration.disarm();
        drop(lifecycle);
        Ok(Run::new(
            Arc::clone(&self.inner),
            self.thread_id.clone(),
            turn_id,
            channels,
            self.turn_timeout,
        ))
    }

    /// Unsubscribes this thread from app-server.
    ///
    /// The active run must first reach a terminal state or be interrupted and drained.
    pub async fn close(&self) -> Result<()> {
        let mut lifecycle = self.lifecycle.lock().await;
        if *lifecycle == SessionLifecycle::Closed {
            return Ok(());
        }
        if self.active.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session::close",
                "cannot close a session with an active run",
            ));
        }
        self.inner.close_session(&self.thread_id).await?;
        *lifecycle = SessionLifecycle::Closed;
        Ok(())
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("thread_id", &self.thread_id)
            .field("cwd", &self.cwd)
            .field("sandbox", &self.sandbox)
            .field("ephemeral", &self.ephemeral)
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Event stream and control handle for one Codex turn.
pub struct Run {
    inner: Arc<ClientInner>,
    thread_id: String,
    turn_id: String,
    events: mpsc::Receiver<Event>,
    terminal: watch::Receiver<Option<Result<RunResult>>>,
    active: Arc<AtomicBool>,
    abandoned: Arc<AtomicBool>,
    control: Arc<RunControl>,
    terminal_delivered: bool,
}

impl Run {
    pub(crate) fn new(
        inner: Arc<ClientInner>,
        thread_id: String,
        turn_id: String,
        channels: RunChannels,
        turn_timeout: Duration,
    ) -> Self {
        inner.arm_run_deadline(
            thread_id.clone(),
            turn_id.clone(),
            Arc::clone(&channels.control),
            turn_timeout,
        );
        Self {
            inner,
            thread_id,
            turn_id,
            events: channels.events,
            terminal: channels.terminal,
            active: channels.active,
            abandoned: channels.abandoned,
            control: channels.control,
            terminal_delivered: false,
        }
    }

    /// Returns the Codex turn identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.turn_id
    }

    /// Returns the parent Codex thread identifier.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Receives the next event. Terminal completion is delivered exactly once.
    pub async fn next_event(&mut self) -> Option<Result<Event>> {
        if self.terminal_delivered {
            return None;
        }
        if let Some(event) = self.events.recv().await {
            return Some(Ok(event));
        }
        let terminal = loop {
            if let Some(result) = self.terminal.borrow().clone() {
                break result;
            }
            if self.terminal.changed().await.is_err() {
                break Err(Error::new(
                    ErrorKind::Disconnected,
                    "Run::next_event",
                    "terminal state channel closed without a result",
                ));
            }
        };
        self.terminal_delivered = true;
        Some(Ok(match terminal {
            Ok(result) => Event::Completed(result),
            Err(error) => Event::Failed(error),
        }))
    }

    /// Requests interruption of this turn. The caller must continue draining events
    /// until the terminal event arrives.
    pub async fn interrupt(&self) -> Result<()> {
        if !self.active.load(Ordering::Acquire) && !self.control.interrupt_started() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Run::interrupt",
                "turn has already reached a terminal state",
            ));
        }
        self.inner
            .interrupt_run(&self.thread_id, &self.turn_id, &self.control)
            .await
    }

    /// Drains the run to its terminal result.
    ///
    /// Approval and user-input requests are resolved with their fail-closed response.
    pub async fn wait(mut self) -> Result<RunResult> {
        while let Some(event) = self.next_event().await {
            match event? {
                Event::ApprovalRequested(request) => request.deny().await?,
                Event::Completed(result) => return Ok(result),
                Event::Failed(error) => return Err(error),
                _ => {}
            }
        }
        Err(Error::new(
            ErrorKind::Disconnected,
            "Run::wait",
            "run event stream ended without a terminal result",
        ))
    }
}

impl fmt::Debug for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Run")
            .field("thread_id", &self.thread_id)
            .field("turn_id", &self.turn_id)
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if !self.active.load(Ordering::Acquire) || self.abandoned.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let inner = Arc::clone(&self.inner);
            let thread_id = self.thread_id.clone();
            let turn_id = self.turn_id.clone();
            let control = Arc::clone(&self.control);
            handle.spawn(async move {
                inner.abandon_run(&thread_id, &turn_id, control).await;
            });
        } else {
            self.inner.begin_force_shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionOptions, SessionRegistry};
    use crate::ErrorKind;
    use std::time::Duration;

    #[test]
    fn loaded_thread_ids_have_single_local_owner() {
        let registry = SessionRegistry::new();
        assert!(registry.insert("thread-1".to_owned()));
        assert!(!registry.insert("thread-1".to_owned()));
        assert!(registry.contains("thread-1"));
        assert_eq!(registry.snapshot(), vec!["thread-1".to_owned()]);
        assert!(registry.remove("thread-1"));
        assert!(!registry.contains("thread-1"));
    }

    #[test]
    fn session_bounds_reject_zero_or_oversized_values() {
        let timeout = SessionOptions::read_only(".")
            .with_turn_timeout(Duration::ZERO)
            .validate()
            .expect_err("zero timeout");
        assert_eq!(timeout.kind(), ErrorKind::InvalidInput);

        let output = SessionOptions::read_only(".")
            .with_maximum_output_bytes(64 * 1024 * 1024 + 1)
            .validate()
            .expect_err("oversized output");
        assert_eq!(output.kind(), ErrorKind::InvalidInput);
    }
}
