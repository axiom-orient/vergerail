//! Codex app-server client and routing core.

mod router;

use crate::account::{Account, Login, LoginMethod, LoginRegistry, LoginWait};
use crate::config::CodexConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::event::{Diagnostic, DiagnosticBuffer, RunResult, TurnAudit};
use crate::image::{
    ChatGptImageAuth, DirectImageRequest, DirectImageResponse, ImageEndpointError,
    generate_via_endpoint,
};
use crate::model::Model;
use crate::private::connection::ConnectionLifecycle;
use crate::private::process::ProcessHandle;
use crate::private::protocol::{
    protocol_field, required_non_empty_string, required_string, turn_audit,
    validate_unsubscribe_response,
};
use crate::private::redact::redact_line;
use crate::private::request::{RequestCancellation, RequestRegistry, TimeoutDisposition};
use crate::private::wire;
use crate::session::{
    DeferredRunNotification, InterruptCompletionGuard, PreStartFailureTransition, ReplayTransition,
    RunChannels, RunControl, RunRegistry, Sandbox, Session, SessionOptions, SessionRegistry,
    StartTurnTransition,
};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

/// Connected, pinned Codex app-server runtime.
pub struct Codex {
    inner: Arc<ClientInner>,
}

pub(crate) struct ClientInner {
    config: CodexConfig,
    process: ProcessHandle,
    image_endpoint: Mutex<String>,
    requests: RequestRegistry,
    runs: RunRegistry,
    sessions: SessionRegistry,
    logins: LoginRegistry,
    diagnostics: DiagnosticBuffer,
    connection: ConnectionLifecycle,
    router_task: Mutex<Option<JoinHandle<()>>>,
}

struct LoginWaiterGuard<'a> {
    registry: &'a LoginRegistry,
    login_id: &'a str,
    waiter_id: u64,
}

impl<'a> LoginWaiterGuard<'a> {
    fn new(registry: &'a LoginRegistry, login_id: &'a str, waiter_id: u64) -> Self {
        Self {
            registry,
            login_id,
            waiter_id,
        }
    }
}

impl Drop for LoginWaiterGuard<'_> {
    fn drop(&mut self) {
        self.registry.remove_waiter(self.login_id, self.waiter_id);
    }
}

struct EphemeralSessionCleanupGuard {
    inner: Arc<ClientInner>,
    thread_id: String,
    armed: bool,
}

impl EphemeralSessionCleanupGuard {
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

impl Drop for EphemeralSessionCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let inner = Arc::clone(&self.inner);
            let thread_id = self.thread_id.clone();
            handle.spawn(async move {
                inner.cleanup_abandoned_ephemeral_session(thread_id).await;
            });
        } else {
            self.inner.begin_force_shutdown();
        }
    }
}

struct PendingCancellationGuard {
    inner: Weak<ClientInner>,
    cancellation: RequestCancellation,
    armed: bool,
}

impl PendingCancellationGuard {
    fn new(inner: &Arc<ClientInner>, cancellation: RequestCancellation) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingCancellationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Publish cancellation synchronously. The router can otherwise remove
        // a successful non-idempotent response before the asynchronous cleanup
        // task observes the pending entry, orphaning the remote side effect.
        self.cancellation.mark_cancelled();
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let cancellation = self.cancellation.clone();
            handle.spawn(async move {
                inner.cancel_aborted_request(cancellation).await;
            });
        } else {
            inner.begin_force_shutdown();
        }
    }
}

struct KnownTurnGuard {
    inner: Weak<ClientInner>,
    thread_id: String,
    turn_id: String,
    armed: bool,
}

impl KnownTurnGuard {
    fn new(inner: &Arc<ClientInner>, thread_id: &str, turn_id: &str) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            thread_id: thread_id.to_owned(),
            turn_id: turn_id.to_owned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for KnownTurnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let thread_id = self.thread_id.clone();
            let turn_id = self.turn_id.clone();
            handle.spawn(async move {
                inner
                    .disconnect(Error::new(
                        ErrorKind::OutcomeUnknown,
                        "turn.start",
                        format!(
                            "turn '{turn_id}' for thread '{thread_id}' was created, but the caller cancelled before Vergerail could commit the run handle; the runtime was terminated and the turn was not retried"
                        ),
                    ))
                    .await;
            });
        } else {
            inner.begin_force_shutdown();
        }
    }
}

impl Codex {
    /// Verifies the exact runtime package, starts app-server with the standard
    /// Codex account, and completes the stable initialize handshake.
    pub async fn connect(config: CodexConfig) -> Result<Self> {
        config.validate()?;
        let runtime = config.runtime.verify().await?;
        let (process, process_events) = ProcessHandle::spawn(&runtime, &config).await?;
        let inner = Arc::new(ClientInner {
            config,
            process,
            image_endpoint: Mutex::new(crate::image::CHATGPT_IMAGE_GENERATION_ENDPOINT.to_owned()),
            requests: RequestRegistry::new(),
            runs: RunRegistry::new(),
            sessions: SessionRegistry::new(),
            logins: LoginRegistry::new(),
            diagnostics: DiagnosticBuffer::new(),
            connection: ConnectionLifecycle::new(),
            router_task: Mutex::new(None),
        });
        let task = tokio::spawn(router::router_loop(Arc::downgrade(&inner), process_events));
        *inner.router_task() = Some(task);

        if let Err(error) = inner.initialize().await {
            return match inner.shutdown().await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(error.with_related_error(
                    "client cleanup after initialization failure also failed",
                    &cleanup_error,
                )),
            };
        }
        Ok(Self { inner })
    }

    /// Reads the current account state from app-server.
    pub async fn account(&self) -> Result<Account> {
        self.inner.account().await
    }

    /// Starts a managed ChatGPT login flow.
    pub async fn login(&self, method: LoginMethod) -> Result<Login> {
        self.inner.login(method).await
    }

    /// Logs out the shared standard Codex account.
    pub async fn logout(&self) -> Result<()> {
        self.inner.logout().await
    }

    /// Returns all models visible to the current account.
    pub async fn models(&self) -> Result<Vec<Model>> {
        self.inner.models().await
    }

    /// Generates exactly one validated PNG using authentication exported by
    /// the official app-server. The image endpoint is retried once only after
    /// that endpoint explicitly returns HTTP 401.
    pub async fn generate_image(&self, request: DirectImageRequest) -> Result<DirectImageResponse> {
        self.inner.generate_image(request).await
    }

    #[cfg(test)]
    pub(crate) fn set_image_endpoint_for_test(&mut self, endpoint: impl Into<String>) {
        *self
            .inner
            .image_endpoint
            .lock()
            .expect("test image endpoint lock") = endpoint.into();
    }

    /// Creates a new Codex session.
    pub async fn session(&self, options: SessionOptions) -> Result<Session> {
        self.inner.create_session(options).await
    }

    /// Resumes a persisted Codex thread.
    pub async fn resume(
        &self,
        thread_id: impl Into<String>,
        options: SessionOptions,
    ) -> Result<Session> {
        self.inner.resume_session(thread_id.into(), options).await
    }

    /// Executes one ephemeral read-only run and returns its terminal result.
    ///
    /// Interactive approvals are fail-closed. Workspace-write sessions must use
    /// [`Codex::session`] and consume the returned run events explicitly.
    pub async fn run(
        &self,
        prompt: impl Into<String>,
        options: SessionOptions,
    ) -> Result<RunResult> {
        if options.sandbox() != Sandbox::ReadOnly {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Codex::run",
                "one-shot run only supports read-only sandbox; use an interactive session for writes",
            ));
        }
        let session = self.inner.create_session(options.ephemeral()).await?;
        let mut cleanup =
            EphemeralSessionCleanupGuard::new(Arc::clone(&self.inner), session.id().to_owned());
        let run = session.start(prompt).await;
        let result = match run {
            Ok(run) => run.wait().await,
            Err(error) => Err(error),
        };
        let close_result = session.close().await;
        // Only a successful unsubscribe resolves the ephemeral thread owner. A
        // returned cleanup failure remains visible to the caller while the armed
        // guard retries once through the same bounded recovery path; a repeated
        // failure terminates the connection instead of leaving an ownerless thread.
        if close_result.is_ok() {
            cleanup.disarm();
        }
        match (result, close_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => {
                Err(error
                    .with_related_error("ephemeral session cleanup also failed", &cleanup_error))
            }
            (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Removes and returns bounded diagnostics not associated with a run.
    pub async fn take_diagnostics(&self) -> Vec<Diagnostic> {
        self.inner.diagnostics.take()
    }

    /// Gracefully stops all tracked sessions and the app-server process.
    pub async fn shutdown(self) -> Result<()> {
        // Once polled, shutdown owns its cleanup task independently of the
        // caller future. Dropping the caller's future must not strand the
        // connection in `closing` while live Session/Login/Run handles keep
        // ClientInner and the child process alive.
        let inner = self.inner;
        let shutdown = tokio::spawn(async move { inner.shutdown().await });
        shutdown.await.map_err(|error| {
            Error::new(
                ErrorKind::Shutdown,
                "Codex::shutdown",
                format!("shutdown task failed: {error}"),
            )
        })?
    }
}

fn remaining_timeout(deadline: Instant) -> std::time::Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        std::time::Duration::from_millis(1)
    } else {
        remaining
    }
}

impl ClientInner {
    fn router_task(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        // The mutex protects only handle ownership. Shutdown takes the handle
        // out before awaiting the task.
        self.router_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) async fn lock_session_lifecycle(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.sessions.lock_lifecycle().await
    }

    async fn initialize(self: &Arc<Self>) -> Result<()> {
        let response = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": self.config.client_name,
                        "title": self.config.client_title,
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": false,
                        "requestAttestation": false
                    }
                }),
                false,
                "initialize",
            )
            .await?;
        if !response.is_object() {
            return Err(Error::new(
                ErrorKind::Protocol,
                "initialize",
                "initialize result must be an object",
            ));
        }
        self.notify("initialized", Value::Null).await
    }

    pub(crate) async fn account(self: &Arc<Self>) -> Result<Account> {
        let response = self
            .request(
                "account/read",
                json!({"refreshToken": false}),
                false,
                "account.read",
            )
            .await?;
        Self::parse_account_response(&response)
    }

    fn parse_account_response(response: &Value) -> Result<Account> {
        let requires_openai_auth = response
            .get("requiresOpenaiAuth")
            .and_then(Value::as_bool)
            .ok_or_else(|| protocol_field("account.read", "requiresOpenaiAuth"))?;
        let Some(account) = response.get("account") else {
            return Ok(Account::SignedOut {
                requires_openai_auth,
            });
        };
        if account.is_null() {
            return Ok(Account::SignedOut {
                requires_openai_auth,
            });
        }
        match account.get("type").and_then(Value::as_str) {
            Some("chatgpt") => {
                let email = match account.get("email") {
                    Some(Value::Null) => None,
                    Some(Value::String(email)) => Some(email.clone()),
                    _ => return Err(protocol_field("account.read", "account.email")),
                };
                Ok(Account::ChatGpt {
                    email,
                    plan: required_string(account, "planType", "account.read")?,
                })
            }
            Some(other) => Err(Error::new(
                ErrorKind::Authentication,
                "account.read",
                format!("the standard Codex account uses unsupported account type '{other}'"),
            )),
            None => Err(protocol_field("account.read", "account.type")),
        }
    }

    async fn login(self: &Arc<Self>, method: LoginMethod) -> Result<Login> {
        let params = match method {
            LoginMethod::Browser => json!({"type": "chatgpt"}),
            LoginMethod::DeviceCode => json!({"type": "chatgptDeviceCode"}),
        };
        let response = self
            .request("account/login/start", params, true, "account.login.start")
            .await?;
        let login_id = match required_non_empty_string(&response, "loginId", "account.login.start")
        {
            Ok(login_id) => login_id,
            Err(error) => {
                self.disconnect(error.clone()).await;
                return Err(error);
            }
        };
        match response.get("type").and_then(Value::as_str) {
            Some("chatgpt") => {
                let auth_url =
                    match required_non_empty_string(&response, "authUrl", "account.login.start") {
                        Ok(auth_url) => auth_url,
                        Err(error) => {
                            self.disconnect(error.clone()).await;
                            return Err(error);
                        }
                    };
                self.register_login(&login_id).await?;
                Ok(Login::browser(Arc::clone(self), login_id, auth_url))
            }
            Some("chatgptDeviceCode") => {
                let verification_url = match required_non_empty_string(
                    &response,
                    "verificationUrl",
                    "account.login.start",
                ) {
                    Ok(verification_url) => verification_url,
                    Err(error) => {
                        self.disconnect(error.clone()).await;
                        return Err(error);
                    }
                };
                let user_code =
                    match required_non_empty_string(&response, "userCode", "account.login.start") {
                        Ok(user_code) => user_code,
                        Err(error) => {
                            self.disconnect(error.clone()).await;
                            return Err(error);
                        }
                    };
                self.register_login(&login_id).await?;
                Ok(Login::device_code(
                    Arc::clone(self),
                    login_id,
                    verification_url,
                    user_code,
                ))
            }
            Some(other) => {
                let error = Error::new(
                    ErrorKind::Protocol,
                    "account.login.start",
                    format!("unexpected login response type '{other}'"),
                );
                self.disconnect(error.clone()).await;
                Err(error)
            }
            None => {
                let error = protocol_field("account.login.start", "type");
                self.disconnect(error.clone()).await;
                Err(error)
            }
        }
    }

    pub(crate) async fn wait_login(self: &Arc<Self>, login_id: &str) -> Result<()> {
        let (waiter_id, receiver) = match self.logins.wait(login_id)? {
            LoginWait::Completed(result) => return result,
            LoginWait::Pending {
                waiter_id,
                receiver,
            } => (waiter_id, receiver),
        };
        // Keep waiter cleanup synchronous and cancellation-safe. Dropping this
        // future before its timeout must not leave an unreachable sender in the
        // registry for the lifetime of the login handle.
        let _waiter = LoginWaiterGuard::new(&self.logins, login_id, waiter_id);
        match timeout(self.config.login_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::new(
                ErrorKind::Disconnected,
                "account.login.wait",
                "login waiter was closed",
            )),
            Err(_) => Err(Error::timeout(
                "account.login.wait",
                self.config.login_timeout,
            )),
        }
    }

    pub(crate) async fn cancel_login(self: &Arc<Self>, login_id: &str) -> Result<()> {
        let response = self
            .request(
                "account/login/cancel",
                json!({"loginId": login_id}),
                false,
                "account.login.cancel",
            )
            .await?;
        match response.get("status").and_then(Value::as_str) {
            Some("canceled" | "notFound") => {
                let cancellation = Err(Error::new(
                    ErrorKind::Authentication,
                    "account.login.wait",
                    "managed ChatGPT login was canceled or is no longer active",
                ));
                self.complete_login(login_id, cancellation);
                Ok(())
            }
            Some(other) => Err(Error::new(
                ErrorKind::Protocol,
                "account.login.cancel",
                format!("unexpected cancel status '{other}'"),
            )),
            None => Err(protocol_field("account.login.cancel", "status")),
        }
    }

    async fn logout(self: &Arc<Self>) -> Result<()> {
        let response = self
            .request("account/logout", json!({}), false, "account.logout")
            .await?;
        if response.is_object() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Protocol,
                "account.logout",
                "logout result must be an object",
            ))
        }
    }

    async fn models(self: &Arc<Self>) -> Result<Vec<Model>> {
        let mut output = Vec::new();
        let mut cursor: Option<String> = None;
        let mut observed = HashSet::new();
        for _ in 0..1000 {
            let response = self
                .request(
                    "model/list",
                    json!({"cursor": cursor, "limit": 100, "includeHidden": false}),
                    false,
                    "model.list",
                )
                .await?;
            let data = response
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| protocol_field("model.list", "data"))?;
            for value in data {
                output.push(Model {
                    id: required_non_empty_string(value, "id", "model.list")?,
                    model: required_non_empty_string(value, "model", "model.list")?,
                    display_name: required_non_empty_string(value, "displayName", "model.list")?,
                    description: required_string(value, "description", "model.list")?,
                    hidden: value
                        .get("hidden")
                        .and_then(Value::as_bool)
                        .ok_or_else(|| protocol_field("model.list", "data[].hidden"))?,
                    is_default: value
                        .get("isDefault")
                        .and_then(Value::as_bool)
                        .ok_or_else(|| protocol_field("model.list", "data[].isDefault"))?,
                });
            }
            cursor = match response.get("nextCursor") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
                Some(_) => return Err(protocol_field("model.list", "nextCursor")),
            };
            let Some(next) = cursor.as_ref() else {
                return Ok(output);
            };
            if !observed.insert(next.clone()) {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "model.list",
                    "app-server repeated a pagination cursor",
                ));
            }
        }
        Err(Error::new(
            ErrorKind::Protocol,
            "model.list",
            "model catalog exceeded 1000 pages",
        ))
    }

    async fn generate_image(
        self: &Arc<Self>,
        request: DirectImageRequest,
    ) -> Result<DirectImageResponse> {
        if request.prompt.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "image.generate",
                "image prompt must be non-empty",
            ));
        }
        if request.prompt.len() > 128 * 1024 || request.prompt.contains('\0') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "image.generate",
                "image prompt exceeds the bounded UTF-8 limit",
            ));
        }
        let deadline = Instant::now() + self.config.request_timeout;
        let image_endpoint = self
            .image_endpoint
            .lock()
            .expect("image endpoint lock")
            .clone();
        generate_image_with_retry(
            request,
            deadline,
            |refresh, request_timeout| self.image_auth(refresh, request_timeout),
            |auth, request, request_timeout, turn_id| {
                generate_via_endpoint(
                    image_endpoint.clone(),
                    auth,
                    request,
                    request_timeout,
                    turn_id,
                )
            },
        )
        .await
    }

    async fn image_auth(
        self: &Arc<Self>,
        refresh: bool,
        request_timeout: std::time::Duration,
    ) -> Result<ChatGptImageAuth> {
        let response = self
            .request_with_timeout(
                "getAuthStatus",
                json!({"includeToken": true, "refreshToken": refresh}),
                false,
                "image.auth",
                request_timeout,
            )
            .await?;
        ChatGptImageAuth::from_auth_status(&response)
    }

    pub(crate) async fn create_session(
        self: &Arc<Self>,
        options: SessionOptions,
    ) -> Result<Session> {
        let _lifecycle = self.sessions.lock_lifecycle().await;
        let options = options.validate()?;
        if options.is_image_only() && !self.config.image_generation {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "thread.start",
                "image-only sessions require CodexConfig::with_image_generation(true)",
            ));
        }
        let cwd = canonical_project(options.cwd()).await?;
        let response = self
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "approvalPolicy": options.sandbox().approval_policy(),
                    "approvalsReviewer": "user",
                    "sandbox": options.sandbox().mode(),
                    "ephemeral": options.is_ephemeral(),
                    "model": options.model_value(),
                    "baseInstructions": options.base_instructions(),
                    "developerInstructions": options.developer_instructions(),
                    "config": thread_config(options.is_text_only(), options.is_image_only())
                }),
                true,
                "thread.start",
            )
            .await?;
        let thread_id = match response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        {
            Some(thread_id) => thread_id,
            None => {
                let error = protocol_field("thread.start", "thread.id");
                self.disconnect(error.clone()).await;
                return Err(error);
            }
        };
        let inserted = self.sessions.insert(thread_id.clone());
        if !inserted {
            let error = Error::new(
                ErrorKind::Protocol,
                "thread.start",
                "app-server returned a thread id that is already loaded",
            );
            self.disconnect(error.clone()).await;
            return Err(error);
        }
        Ok(Session::new(Arc::clone(self), thread_id, cwd, &options))
    }

    pub(crate) async fn resume_session(
        self: &Arc<Self>,
        thread_id: String,
        options: SessionOptions,
    ) -> Result<Session> {
        let _lifecycle = self.sessions.lock_lifecycle().await;
        if thread_id.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "thread.resume",
                "thread id must be non-empty",
            ));
        }
        let options = options.validate()?;
        if options.is_image_only() && !self.config.image_generation {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "thread.resume",
                "image-only sessions require CodexConfig::with_image_generation(true)",
            ));
        }
        if options.is_ephemeral() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "thread.resume",
                "a persisted thread cannot be resumed as ephemeral",
            ));
        }
        let already_loaded = self.sessions.contains(&thread_id);
        if already_loaded {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "thread.resume",
                "thread is already loaded by this client",
            ));
        }
        let cwd = canonical_project(options.cwd()).await?;
        let response = self
            .request(
                "thread/resume",
                json!({
                    "threadId": thread_id.clone(),
                    "cwd": cwd,
                    "approvalPolicy": options.sandbox().approval_policy(),
                    "approvalsReviewer": "user",
                    "sandbox": options.sandbox().mode(),
                    "model": options.model_value(),
                    "baseInstructions": options.base_instructions(),
                    "developerInstructions": options.developer_instructions(),
                    "config": thread_config(options.is_text_only(), options.is_image_only())
                }),
                true,
                "thread.resume",
            )
            .await?;
        let observed = match response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        {
            Some(observed) => observed,
            None => {
                let error = protocol_field("thread.resume", "thread.id");
                self.disconnect(error.clone()).await;
                return Err(error);
            }
        };
        if observed != thread_id {
            let error = Error::new(
                ErrorKind::Protocol,
                "thread.resume",
                format!(
                    "app-server resumed thread '{observed}' instead of requested thread '{thread_id}'"
                ),
            );
            self.disconnect(error.clone()).await;
            return Err(error);
        }
        let inserted = self.sessions.insert(observed.clone());
        if !inserted {
            let error = Error::new(
                ErrorKind::Protocol,
                "thread.resume",
                "app-server resumed a thread that is already loaded by this client",
            );
            self.disconnect(error.clone()).await;
            return Err(error);
        }
        Ok(Session::new(Arc::clone(self), observed, cwd, &options))
    }

    pub(crate) async fn audit_turn(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<TurnAudit> {
        let response = self
            .request(
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": true}),
                false,
                "thread.read",
            )
            .await?;
        turn_audit(&response, thread_id, turn_id)
    }

    pub(crate) async fn close_session(self: &Arc<Self>, thread_id: &str) -> Result<()> {
        let _lifecycle = self.sessions.lock_lifecycle().await;
        if self.runs.contains(thread_id) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "thread.unsubscribe",
                "cannot unsubscribe a thread with an active run",
            ));
        }
        let response = self
            .request(
                "thread/unsubscribe",
                json!({"threadId": thread_id}),
                false,
                "thread.unsubscribe",
            )
            .await?;
        validate_unsubscribe_response(&response)?;
        self.sessions.remove(thread_id);
        Ok(())
    }

    async fn cleanup_abandoned_ephemeral_session(self: Arc<Self>, thread_id: String) {
        let _lifecycle = self.sessions.lock_lifecycle().await;
        if !self.sessions.contains(&thread_id) {
            return;
        }
        if self.connection.is_disconnected() {
            self.sessions.remove(&thread_id);
            return;
        }

        if let Some((turn_id, control)) = self.runs.cancel_registered(&thread_id)
            && let Err(error) = self
                .interrupt_and_wait_for_provider_terminal(&thread_id, &turn_id, &control)
                .await
        {
            let failure = Error::new(
                ErrorKind::Disconnected,
                "Codex::run",
                "failed to terminate a cancelled one-shot run",
            )
            .with_related_error("run cleanup also failed", &error);
            self.disconnect(failure).await;
            return;
        }

        if self.connection.is_disconnected() {
            self.sessions.remove(&thread_id);
            return;
        }
        let cleanup = self
            .request_internal(
                "thread/unsubscribe",
                json!({"threadId": thread_id.clone()}),
                false,
                "thread.unsubscribe",
                true,
            )
            .await
            .and_then(|response| validate_unsubscribe_response(&response));
        match cleanup {
            Ok(()) => {
                self.sessions.remove(&thread_id);
            }
            Err(error) => {
                let failure = Error::new(
                    ErrorKind::Disconnected,
                    "Codex::run",
                    "failed to unsubscribe a cancelled one-shot session",
                )
                .with_related_error("session cleanup also failed", &error);
                self.disconnect(failure).await;
            }
        }
    }

    pub(crate) fn register_run(
        self: &Arc<Self>,
        thread_id: &str,
        active: Arc<AtomicBool>,
        maximum_output_bytes: usize,
    ) -> Result<RunChannels> {
        self.runs.register(
            thread_id,
            active,
            self.config.event_capacity,
            maximum_output_bytes,
        )
    }

    pub(crate) fn arm_run_deadline(
        self: &Arc<Self>,
        thread_id: String,
        turn_id: String,
        control: Arc<RunControl>,
        turn_timeout: std::time::Duration,
    ) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let mut provider_terminal = control.subscribe_provider_terminal();
            let terminal = async {
                loop {
                    if *provider_terminal.borrow() {
                        return;
                    }
                    if provider_terminal.changed().await.is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                () = sleep(turn_timeout) => {
                    if inner.runs.fail_active(
                        &thread_id,
                        &turn_id,
                        &control,
                        Error::timeout("turn.run", turn_timeout),
                    ) {
                        inner.interrupt_failed_run(&thread_id, &turn_id, control);
                    }
                }
                () = terminal => {}
            }
        });
    }

    pub(crate) async fn start_turn(
        self: &Arc<Self>,
        thread_id: &str,
        cwd: &PathBuf,
        sandbox: Sandbox,
        reasoning: crate::session::ReasoningEffort,
        prompt: String,
        output_schema: Option<Value>,
    ) -> Result<String> {
        let sandbox_policy = match sandbox {
            Sandbox::ReadOnly => json!({
                "type": "readOnly",
                "networkAccess": false
            }),
            Sandbox::WorkspaceWrite => json!({
                "type": "workspaceWrite",
                "writableRoots": [cwd],
                "networkAccess": false,
                "excludeTmpdirEnvVar": true,
                "excludeSlashTmp": true
            }),
        };
        let mut params = json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt, "text_elements": []}],
            "cwd": cwd,
            "effort": reasoning.value(),
            "approvalPolicy": sandbox.approval_policy(),
            "approvalsReviewer": "user",
            "sandboxPolicy": sandbox_policy
        });
        if let Some(schema) = output_schema {
            params["outputSchema"] = schema;
        }
        let response = self.request("turn/start", params, true, "turn.start").await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.fail_registered_run_before_turn(thread_id, error.clone())
                    .await;
                return Err(error);
            }
        };
        let turn_id = match response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        {
            Some(turn_id) => turn_id,
            None => {
                let error = protocol_field("turn.start", "turn.id");
                self.disconnect(error.clone()).await;
                return Err(error);
            }
        };
        let mut known_turn = KnownTurnGuard::new(self, thread_id, &turn_id);
        match self.runs.acknowledge_start(thread_id, &turn_id) {
            StartTurnTransition::Replay(deferred) => {
                self.replay_deferred_run_notifications(thread_id, &turn_id, deferred)
                    .await;
            }
            StartTurnTransition::CompletedBeforeAcknowledgement => {}
            StartTurnTransition::MissingRoute => {
                return Err(Error::new(
                    ErrorKind::Disconnected,
                    "turn.start",
                    "run route disappeared before turn/start completed",
                ));
            }
            StartTurnTransition::TerminalTurnMismatch { expected } => {
                let error = Error::new(
                    ErrorKind::Protocol,
                    "turn.start",
                    format!(
                        "turn/start acknowledged turn '{turn_id}' after a terminal notification for turn '{expected}'"
                    ),
                );
                self.disconnect(error.clone()).await;
                return Err(error);
            }
            StartTurnTransition::AlreadyAcknowledged => {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "turn.start",
                    "run route acknowledged turn/start more than once",
                ));
            }
        }
        known_turn.disarm();
        Ok(turn_id)
    }

    async fn replay_deferred_run_notifications(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        mut deferred: VecDeque<DeferredRunNotification>,
    ) {
        loop {
            while let Some(notification) = deferred.pop_front() {
                match notification {
                    DeferredRunNotification::Event {
                        turn_id: observed,
                        source_method,
                        event,
                    } => {
                        self.route_run_event(
                            thread_id,
                            Some(&observed),
                            &source_method,
                            *event,
                            false,
                        );
                    }
                    DeferredRunNotification::Terminal(params) => {
                        self.route_terminal_notification(&params, false).await;
                    }
                }
            }

            match self.runs.replay_transition(thread_id, turn_id) {
                ReplayTransition::Next(queued) => deferred = queued,
                ReplayTransition::Active | ReplayTransition::Stopped => return,
            }
        }
    }

    pub(crate) async fn interrupt_run(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        control: &Arc<RunControl>,
    ) -> Result<()> {
        let mut result = control.subscribe_interrupt_result();
        if control.try_start_interrupt() {
            let inner = Arc::clone(self);
            let thread_id = thread_id.to_owned();
            let turn_id = turn_id.to_owned();
            let control = Arc::clone(control);
            tokio::spawn(async move {
                let completion = InterruptCompletionGuard::new(Arc::clone(&control));
                let mut provider_terminal = control.subscribe_provider_terminal();
                let request = inner.interrupt_turn_internal(&thread_id, &turn_id, true);
                tokio::pin!(request);
                let result = if *provider_terminal.borrow() {
                    Ok(())
                } else {
                    tokio::select! {
                        result = &mut request => result,
                        changed = provider_terminal.changed() => match changed {
                            Ok(()) if *provider_terminal.borrow() => Ok(()),
                            Ok(()) => Err(Error::new(
                                ErrorKind::Protocol,
                                "turn.completed",
                                "provider terminal channel changed without a terminal observation",
                            )),
                            Err(_) => Err(Error::new(
                                ErrorKind::Disconnected,
                                "turn.completed",
                                "provider terminal channel closed without a terminal observation",
                            )),
                        },
                    }
                };
                completion.complete(result);
            });
        } else if control.provider_terminal_observed() && !control.interrupt_started() {
            return Ok(());
        }

        loop {
            let observed = result.borrow().clone();
            if let Some(result) = observed {
                return result;
            }
            if result.changed().await.is_err() {
                return Err(Error::new(
                    ErrorKind::Disconnected,
                    "turn.interrupt",
                    "interrupt result channel closed without a result",
                ));
            }
        }
    }

    async fn interrupt_turn_internal(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        allow_closing: bool,
    ) -> Result<()> {
        let response = self
            .request_internal(
                "turn/interrupt",
                json!({"threadId": thread_id, "turnId": turn_id}),
                false,
                "turn.interrupt",
                allow_closing,
            )
            .await?;
        if response.is_object() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Protocol,
                "turn.interrupt",
                "interrupt result must be an object",
            ))
        }
    }

    async fn wait_for_provider_terminal(
        &self,
        thread_id: &str,
        turn_id: &str,
        control: &Arc<RunControl>,
    ) -> Result<()> {
        let mut terminal = control.subscribe_provider_terminal();
        let wait = async {
            loop {
                let observed = *terminal.borrow();
                if observed {
                    return Ok(());
                }
                if terminal.changed().await.is_err() {
                    return Err(Error::new(
                        ErrorKind::Disconnected,
                        "turn.completed",
                        "provider terminal channel closed without a terminal observation",
                    ));
                }
            }
        };
        timeout(self.config.shutdown_timeout, wait)
            .await
            .map_err(|_| {
                Error::new(
                    ErrorKind::Timeout,
                    "turn.completed",
                    format!(
                        "turn '{turn_id}' for thread '{thread_id}' did not reach a provider terminal state within {} ms after interruption",
                        self.config.shutdown_timeout.as_millis()
                    ),
                )
            })?
    }

    async fn interrupt_and_wait_for_provider_terminal(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        control: &Arc<RunControl>,
    ) -> Result<()> {
        let interrupt = self.interrupt_run(thread_id, turn_id, control);
        let terminal = self.wait_for_provider_terminal(thread_id, turn_id, control);
        tokio::pin!(interrupt);
        tokio::pin!(terminal);

        tokio::select! {
            terminal_result = &mut terminal => terminal_result,
            interrupt_result = &mut interrupt => {
                match terminal.await {
                    Ok(()) => Ok(()),
                    Err(terminal_error) => match interrupt_result {
                        Ok(()) => Err(terminal_error),
                        Err(interrupt_error) => Err(interrupt_error.with_related_error(
                            "provider terminal wait also failed",
                            &terminal_error,
                        )),
                    },
                }
            }
        }
    }

    fn interrupt_failed_run(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        control: Arc<RunControl>,
    ) {
        let inner = Arc::clone(self);
        let thread_id = thread_id.to_owned();
        let turn_id = turn_id.to_owned();
        tokio::spawn(async move {
            if inner.connection.is_disconnected() {
                return;
            }
            if let Err(error) = inner
                .interrupt_and_wait_for_provider_terminal(&thread_id, &turn_id, &control)
                .await
            {
                if inner.connection.is_disconnected() {
                    return;
                }
                inner
                    .disconnect(Error::new(
                        ErrorKind::Disconnected,
                        "run.events",
                        format!(
                            "failed to terminate turn '{turn_id}' for thread '{thread_id}' after the run route failed: {error}"
                        ),
                    ))
                    .await;
            }
        });
    }

    pub(crate) async fn abandon_run(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        control: Arc<RunControl>,
    ) {
        if self.connection.is_disconnected() {
            return;
        }
        if !self
            .runs
            .tracks_abandoned_turn(thread_id, turn_id, &control)
        {
            return;
        }

        if let Err(error) = self
            .interrupt_and_wait_for_provider_terminal(thread_id, turn_id, &control)
            .await
        {
            if self.connection.is_disconnected() {
                return;
            }
            self.disconnect(Error::new(
                ErrorKind::Disconnected,
                "run.drop",
                format!("failed to terminate an abandoned run: {error}"),
            ))
            .await;
        }
    }

    pub(crate) fn begin_force_shutdown(&self) {
        self.process.begin_force_shutdown();
    }

    async fn fail_registered_run_before_turn(self: &Arc<Self>, thread_id: &str, error: Error) {
        if matches!(
            self.runs.fail_before_start(thread_id, error),
            PreStartFailureTransition::ProviderTurnOwned
        ) {
            self.disconnect(Error::new(
                ErrorKind::Protocol,
                "turn.start",
                "turn/start failed before ownership was established, but the run route already owned a provider turn",
            ))
            .await;
        }
    }

    pub(crate) async fn cancel_registered_run(self: Arc<Self>, thread_id: String) {
        let active_turn = self.runs.cancel_registered(&thread_id);
        if let Some((turn_id, control)) = active_turn {
            self.abandon_run(&thread_id, &turn_id, control).await;
        }
    }

    async fn cancel_aborted_request(self: Arc<Self>, cancellation: RequestCancellation) {
        if self.requests.cancel(&cancellation) {
            self.disconnect(Error::new(
                ErrorKind::OutcomeUnknown,
                cancellation.operation(),
                "the caller cancelled before Vergerail could commit a dispatched non-idempotent response; the runtime was terminated and the request was not retried",
            ))
            .await;
        }
    }

    async fn request(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
        non_idempotent: bool,
        operation: &'static str,
    ) -> Result<Value> {
        self.request_internal(method, params, non_idempotent, operation, false)
            .await
    }

    async fn request_with_timeout(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
        non_idempotent: bool,
        operation: &'static str,
        request_timeout: std::time::Duration,
    ) -> Result<Value> {
        self.request_internal_with_timeout(
            method,
            params,
            non_idempotent,
            operation,
            false,
            request_timeout,
        )
        .await
    }

    async fn request_internal(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
        non_idempotent: bool,
        operation: &'static str,
        allow_closing: bool,
    ) -> Result<Value> {
        self.request_internal_with_timeout(
            method,
            params,
            non_idempotent,
            operation,
            allow_closing,
            self.config.request_timeout,
        )
        .await
    }

    async fn request_internal_with_timeout(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
        non_idempotent: bool,
        operation: &'static str,
        allow_closing: bool,
        request_timeout: std::time::Duration,
    ) -> Result<Value> {
        if self.connection.is_closing() && !allow_closing {
            return Err(Error::new(
                ErrorKind::Shutdown,
                operation,
                "client shutdown has started",
            ));
        }
        if self.connection.is_disconnected() {
            return Err(self.connection.failure().unwrap_or_else(|| {
                Error::new(
                    ErrorKind::Disconnected,
                    operation,
                    "app-server is disconnected",
                )
            }));
        }
        let registration = self.requests.register(operation, non_idempotent)?;
        let numeric = registration.numeric_id;
        let id = registration.id;
        let receiver = registration.receiver;
        let dispatched = registration.dispatched;
        let cancellation_state = registration.cancellation.clone();
        let mut cancellation = PendingCancellationGuard::new(self, registration.cancellation);

        let mut ownership_resolved = false;
        let result = async {
            if let Err(error) = self
                .process
                .send_tracked(
                    wire::request(numeric, method, params),
                    Arc::clone(&dispatched),
                )
                .await
            {
                self.requests.remove(&id);
                if error.kind() == ErrorKind::OutcomeUnknown {
                    let outcome = Error::new(
                        ErrorKind::OutcomeUnknown,
                        operation,
                        "request was dispatched but its write outcome could not be observed; runtime termination was initiated and the request was not retried",
                    );
                    self.disconnect(outcome.clone()).await;
                    return Err(outcome);
                }
                ownership_resolved = !dispatched.load(Ordering::Acquire);
                return Err(
                    Error::new(error.kind(), operation, error.message().to_owned())
                        .with_stderr(error.stderr_tail().map(str::to_owned)),
                );
            }

            match timeout(request_timeout, receiver).await {
                Ok(Ok(result)) => {
                    ownership_resolved = true;
                    result
                }
                Ok(Err(_)) => Err(Error::new(
                    ErrorKind::Disconnected,
                    operation,
                    "response waiter closed before a response arrived",
                )),
                Err(_) => {
                    match self.requests.timeout(&cancellation_state) {
                        TimeoutDisposition::OutcomeUnknown => {
                            let outcome = Error::new(
                                ErrorKind::OutcomeUnknown,
                                operation,
                                format!(
                                    "request was written but no response arrived within {} ms; the runtime was killed and the request was not retried",
                                    request_timeout.as_millis()
                                ),
                            );
                            // Mark the client disconnected before terminating the child. Otherwise
                            // the stdout EOF task can win the race and replace the causal
                            // OutcomeUnknown error with a generic Disconnected error.
                            self.disconnect(outcome.clone()).await;
                            Err(outcome)
                        }
                        TimeoutDisposition::TimedOut => {
                            Err(Error::timeout(operation, request_timeout))
                        }
                    }
                }
            }
        }
        .await;
        if ownership_resolved {
            cancellation.disarm();
        }
        result
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.process.send(wire::notification(method, params)).await
    }

    async fn shutdown(self: &Arc<Self>) -> Result<()> {
        if !self.connection.begin_closing() {
            return Ok(());
        }
        let _lifecycle = self.sessions.lock_lifecycle().await;
        let sessions = self.sessions.snapshot();
        let mut failures = Vec::new();
        if self.connection.is_disconnected() {
            // A forced disconnect has already terminated the process, so remote
            // threads no longer exist. Avoid issuing meaningless RPCs against a
            // dead transport while still joining and checking local resources.
            self.sessions.clear();
        } else {
            for thread_id in sessions {
                let active_turn = self.runs.active_turn(&thread_id);
                if let Some((turn_id, control)) = active_turn
                    && let Err(error) = self
                        .interrupt_and_wait_for_provider_terminal(&thread_id, &turn_id, &control)
                        .await
                {
                    failures.push(error.to_string());
                }
                match self
                    .request_internal(
                        "thread/unsubscribe",
                        json!({"threadId": thread_id}),
                        false,
                        "thread.unsubscribe",
                        true,
                    )
                    .await
                {
                    Ok(response) => match validate_unsubscribe_response(&response) {
                        Ok(()) => {
                            self.sessions.remove(&thread_id);
                        }
                        Err(error) => failures.push(error.to_string()),
                    },
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }
        if let Err(error) = self.process.shutdown().await {
            failures.push(error.to_string());
        }
        let router_task = self.router_task().take();
        if let Some(mut task) = router_task {
            match timeout(self.config.shutdown_timeout, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("router task failed: {error}")),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    failures.push("timed out joining router task; task was aborted".to_owned());
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                Error::new(ErrorKind::Shutdown, "Codex::shutdown", failures.join("; "))
                    .with_stderr(self.process.stderr_tail()),
            )
        }
    }

    async fn register_login(self: &Arc<Self>, login_id: &str) -> Result<()> {
        let inserted = self.logins.register(login_id);
        if inserted {
            if self.connection.is_disconnected() {
                let error = self.connection.failure().unwrap_or_else(|| {
                    Error::new(
                        ErrorKind::Disconnected,
                        "account.login.wait",
                        "app-server disconnected before the login handle was registered",
                    )
                });
                self.complete_login(login_id, Err(error));
            }
            return Ok(());
        }
        let error = Error::new(
            ErrorKind::Protocol,
            "account.login.start",
            "app-server returned a login id that is already active",
        );
        self.disconnect(error.clone()).await;
        Err(error)
    }

    pub(crate) fn release_login(&self, login_id: &str) {
        self.logins.release(login_id);
    }

    fn complete_login(&self, login_id: &str, result: Result<()>) {
        self.logins.complete(login_id, result);
    }

    fn push_diagnostic(&self, method: &str, message: String) {
        self.diagnostics.push(Diagnostic {
            method: method.to_owned(),
            message: redact_line(&message),
        });
    }

    async fn disconnect(&self, mut error: Error) {
        if !self.connection.begin_disconnect(error.clone()) {
            return;
        }
        if let Err(termination) = self.process.force_kill().await {
            error = Error::new(
                error.kind(),
                error.operation(),
                format!(
                    "{}; runtime termination failed: {termination}",
                    error.message()
                ),
            );
        }
        error = error.with_stderr(self.process.stderr_tail_after_close().await);
        self.connection.replace_failure(error.clone());

        self.requests.fail_all(&error);

        self.runs.fail_all(&error);

        self.logins.fail_active(&error);
    }
}

async fn generate_image_with_retry<AuthFactory, AuthFuture, EndpointFactory, EndpointFuture>(
    request: DirectImageRequest,
    deadline: Instant,
    mut auth_factory: AuthFactory,
    mut endpoint_factory: EndpointFactory,
) -> Result<DirectImageResponse>
where
    AuthFactory: FnMut(bool, std::time::Duration) -> AuthFuture,
    AuthFuture: Future<Output = Result<ChatGptImageAuth>>,
    EndpointFactory:
        FnMut(ChatGptImageAuth, DirectImageRequest, std::time::Duration, String) -> EndpointFuture,
    EndpointFuture: Future<Output = std::result::Result<DirectImageResponse, ImageEndpointError>>,
{
    let turn_id = crate::image::image_turn_id();
    let auth = auth_factory(true, remaining_timeout(deadline)).await?;
    match endpoint_factory(
        auth,
        request.clone(),
        remaining_timeout(deadline),
        turn_id.clone(),
    )
    .await
    {
        Ok(response) => Ok(response),
        Err(ImageEndpointError::Unauthorized) => {
            let refreshed = auth_factory(true, remaining_timeout(deadline)).await?;
            match endpoint_factory(refreshed, request, remaining_timeout(deadline), turn_id).await {
                Ok(response) => Ok(response),
                Err(ImageEndpointError::Unauthorized) => Err(Error::new(
                    ErrorKind::Authentication,
                    "image.generate",
                    "official image endpoint rejected refreshed authentication",
                )),
                Err(ImageEndpointError::Failed(error)) => Err(error),
            }
        }
        Err(ImageEndpointError::Failed(error)) => Err(error),
    }
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        self.process.begin_force_shutdown();
    }
}

fn thread_config(text_only: bool, image_only: bool) -> Value {
    if !text_only && !image_only {
        return Value::Null;
    }
    json!({
        "web_search": "disabled",
        "features": {
            "apps": false,
            "goals": false,
            "hooks": false,
            "image_generation": image_only,
            "memories": false,
            "multi_agent": false,
            "plugins": false,
            "remote_plugin": false,
            "shell_tool": false,
            "skill_mcp_dependency_install": false,
            "unified_exec": false
        },
        "apps": {
            "_default": {
                "enabled": false
            }
        },
        "history": {
            "persistence": "none"
        },
        "memories": {
            "generate_memories": false,
            "use_memories": false
        }
    })
}

async fn canonical_project(cwd: &Path) -> Result<PathBuf> {
    let metadata = tokio::fs::symlink_metadata(cwd).await.map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            format!("cannot inspect {}: {error}", cwd.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            "working directory must be a real, non-symlink directory",
        ));
    }
    let canonical = tokio::fs::canonicalize(cwd).await.map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            format!("cannot canonicalize {}: {error}", cwd.display()),
        )
    })?;
    if canonical.to_str().is_none() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            "working directory must be valid UTF-8 for the protocol boundary",
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod waiter_guard_tests {
    use super::*;
    use tokio::sync::oneshot::error::TryRecvError;

    #[test]
    fn dropping_wait_future_registration_removes_its_sender() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        let (waiter_id, mut receiver) = match registry.wait("login-1").expect("waiter") {
            LoginWait::Pending {
                waiter_id,
                receiver,
            } => (waiter_id, receiver),
            LoginWait::Completed(_) => panic!("new login completed unexpectedly"),
        };

        {
            let _guard = LoginWaiterGuard::new(&registry, "login-1", waiter_id);
        }

        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Closed)));
    }

    #[test]
    fn text_only_config_disables_every_execution_and_external_context_surface() {
        let config = thread_config(true, false);
        assert_eq!(config.pointer("/features/shell_tool"), Some(&json!(false)));
        assert_eq!(config.pointer("/features/apps"), Some(&json!(false)));
        assert_eq!(
            config.pointer("/features/image_generation"),
            Some(&json!(false))
        );
        assert_eq!(config.pointer("/features/multi_agent"), Some(&json!(false)));
        assert_eq!(config.get("web_search"), Some(&json!("disabled")));
        assert_eq!(
            config.pointer("/memories/use_memories"),
            Some(&json!(false))
        );
        assert_eq!(thread_config(false, false), Value::Null);
    }

    #[test]
    fn image_only_config_enables_exactly_image_generation() {
        let config = thread_config(false, true);
        assert_eq!(config.pointer("/web_search"), Some(&json!("disabled")));
        assert_eq!(
            config.pointer("/features/image_generation"),
            Some(&json!(true))
        );
        for feature in [
            "apps",
            "goals",
            "hooks",
            "memories",
            "multi_agent",
            "plugins",
            "remote_plugin",
            "shell_tool",
            "skill_mcp_dependency_install",
            "unified_exec",
        ] {
            assert_eq!(
                config.pointer(&format!("/features/{feature}")),
                Some(&json!(false))
            );
        }
        assert_eq!(
            config.pointer("/apps/_default/enabled"),
            Some(&json!(false))
        );
        assert_eq!(config.pointer("/history/persistence"), Some(&json!("none")));
        assert_eq!(
            config.pointer("/memories/generate_memories"),
            Some(&json!(false))
        );
        assert_eq!(
            config.pointer("/memories/use_memories"),
            Some(&json!(false))
        );
    }

    #[tokio::test]
    async fn image_retry_orchestration_refreshes_auth_once_and_reuses_turn_id() {
        let auth_calls = Arc::new(Mutex::new(Vec::<bool>::new()));
        let endpoint_calls = Arc::new(Mutex::new(Vec::<(String, std::time::Duration)>::new()));
        let auth_calls_for_factory = Arc::clone(&auth_calls);
        let auth_factory = move |refresh: bool, request_timeout: std::time::Duration| {
            let auth_calls = Arc::clone(&auth_calls_for_factory);
            let _ = request_timeout;
            auth_calls.lock().expect("auth calls lock").push(refresh);
            let auth = ChatGptImageAuth::from_auth_status(&json!({
                "authMethod": "chatgpt",
                "authToken": "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoidGVzdC1hY2NvdW50In19.sig",
                "requiresOpenaiAuth": true
            }));
            async move { auth }
        };
        let endpoint_calls_for_factory = Arc::clone(&endpoint_calls);
        let endpoint_factory = move |_auth: ChatGptImageAuth,
                                     _request: DirectImageRequest,
                                     request_timeout: std::time::Duration,
                                     turn_id: String| {
            let mut calls = endpoint_calls_for_factory
                .lock()
                .expect("endpoint calls lock");
            calls.push((turn_id, request_timeout));
            let attempt = calls.len();
            drop(calls);
            async move {
                assert!(attempt <= 2, "image retry must not exceed one retry");
                Err(ImageEndpointError::Unauthorized)
            }
        };
        let request = DirectImageRequest {
            model: "gpt-image-1".to_owned(),
            prompt: "test image".to_owned(),
            background: crate::image::ImageBackground::Transparent,
            size: crate::image::ImageSize::Square,
            quality: crate::image::ImageQuality::Low,
        };
        let error = generate_image_with_retry(
            request,
            Instant::now() + std::time::Duration::from_secs(1),
            auth_factory,
            endpoint_factory,
        )
        .await
        .expect_err("second 401 must be terminal");
        assert_eq!(error.kind(), ErrorKind::Authentication);
        assert_eq!(
            *auth_calls.lock().expect("auth calls lock"),
            vec![true, true]
        );
        let calls = endpoint_calls.lock().expect("endpoint calls lock");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, calls[1].0);
        assert!(calls.iter().all(|(_, timeout)| !timeout.is_zero()));
    }

    #[test]
    fn account_read_without_account_field_is_signed_out() {
        assert_eq!(
            ClientInner::parse_account_response(&json!({"requiresOpenaiAuth": true}))
                .expect("absent account is signed out"),
            Account::SignedOut {
                requires_openai_auth: true,
            }
        );
    }

    #[test]
    fn account_read_null_account_is_signed_out() {
        assert_eq!(
            ClientInner::parse_account_response(
                &json!({"requiresOpenaiAuth": false, "account": null})
            )
            .expect("null account is signed out"),
            Account::SignedOut {
                requires_openai_auth: false,
            }
        );
    }
}
