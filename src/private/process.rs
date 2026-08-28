use crate::config::CodexConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::private::codec::{EncodedFrame, JsonLinesReader, JsonLinesWriter};
use crate::private::process_tree;
use crate::private::redact::StderrRing;
use crate::private::wire::{self, Incoming};
use crate::runtime::VerifiedRuntime;
use serde_json::Value;
use std::env;
use std::fmt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, Weak};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};
use tokio::process::Child;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

const MAX_STDERR_LINE_BYTES: usize = 64 * 1024;
const OVERSIZED_STDERR_LINE: &str = "<oversized stderr line omitted>";

pub(crate) enum ProcessEvent {
    Message(Incoming),
    Closed(Error),
}

enum Outbound {
    Frame {
        frame: EncodedFrame,
        ack: oneshot::Sender<Result<()>>,
    },
    Close {
        ack: oneshot::Sender<Result<()>>,
    },
}

#[derive(Clone)]
pub(crate) struct ProcessHandle {
    inner: Arc<ProcessInner>,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessHandle")
            .finish_non_exhaustive()
    }
}

struct ProcessInner {
    outbound: mpsc::Sender<Outbound>,
    child: Mutex<Option<Child>>,
    identity: process_tree::ProcessIdentity,
    guardian_path: std::path::PathBuf,
    guardian_directory: std::path::PathBuf,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    stderr: Arc<StderrCapture>,
    shutdown_started: AtomicBool,
    cleanup_finished: AtomicBool,
    shutdown_timeout: Duration,
    send_timeout: Duration,
    max_frame_bytes: usize,
}

impl ProcessInner {
    fn tasks(&self) -> StdMutexGuard<'_, Vec<JoinHandle<()>>> {
        // Task handles are transferred under this lock and joined only after
        // the guard has been released.
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for ProcessInner {
    fn drop(&mut self) {
        // The owned child has kill-on-drop. Unlink the per-run executable and
        // directory as a final synchronous guard when a Tokio runtime is torn
        // down before asynchronous shutdown can finish.
        let _ = process_tree::remove_guardian(&self.guardian_path);
        let _ = process_tree::remove_guardian_directory(&self.guardian_directory);
    }
}

struct StderrCapture {
    ring: StdMutex<StderrRing>,
    done: AtomicBool,
    notify: Notify,
}

impl StderrCapture {
    fn new(capacity: usize) -> Self {
        Self {
            ring: StdMutex::new(StderrRing::new(capacity)),
            done: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn ring(&self) -> StdMutexGuard<'_, StderrRing> {
        self.ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, line: &str) {
        self.ring().push(line);
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn tail(&self) -> Option<String> {
        self.ring().tail()
    }

    async fn tail_after_close(&self, deadline: Duration) -> Option<String> {
        if !self.done.load(Ordering::Acquire) {
            let notified = self.notify.notified();
            if !self.done.load(Ordering::Acquire) {
                let _ = timeout(deadline, notified).await;
            }
        }
        self.tail()
    }
}

impl ProcessHandle {
    pub(crate) async fn spawn(
        runtime: &VerifiedRuntime,
        config: &CodexConfig,
    ) -> Result<(Self, mpsc::Receiver<ProcessEvent>)> {
        let guardian_directory =
            process_tree::create_guardian_directory(&env::temp_dir(), "vergerail-runtime")
                .map_err(|error| {
                    Error::new(
                        ErrorKind::Process,
                        "process.guardian",
                        format!("cannot create a private guardian directory: {error}"),
                    )
                })?;
        let guardian_path = match process_tree::extract_guardian(&guardian_directory) {
            Ok(path) => path,
            Err(error) => {
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::Process,
                    "process.guardian",
                    format!("cannot materialize the audited macOS guardian: {error}"),
                ));
            }
        };
        let mut command = process_tree::command(&guardian_path, &runtime.entrypoint);
        command
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .arg("--strict-config")
            .current_dir(env::temp_dir())
            .env_remove("CODEX_HOME")
            .env("LOG_FORMAT", "json")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("OPENAI_API_HOST")
            .env_remove("OPENAI_ORGANIZATION")
            .env_remove("OPENAI_PROJECT")
            .env_remove("CODEX_BASE_URL")
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("CODEX_CONNECTORS_TOKEN")
            .env_remove("CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG")
            .env_remove("CODEX_APP_SERVER_MANAGED_CONFIG_PATH")
            .env_remove("CODEX_APP_SERVER_TEST_USER_CONFIG_FILE")
            .env_remove("CODEX_MANAGED_CONFIG_SYSTEM_PATH")
            .env_remove("CODEX_APP_SERVER_LOGIN_CLIENT_ID")
            .env_remove("CODEX_APP_SERVER_LOGIN_ISSUER")
            .env_remove("CODEX_AUTHAPI_BASE_URL")
            .env_remove("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
            .env_remove("CODEX_REVOKE_TOKEN_URL_OVERRIDE")
            .env_remove("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let bundled_path = runtime.root.join("codex-path");
        let mut path_segments = vec![bundled_path];
        if let Some(current) = env::var_os("PATH") {
            path_segments.extend(env::split_paths(&current));
        }
        let path = env::join_paths(path_segments).map_err(|error| {
            let _ = process_tree::remove_guardian(&guardian_path);
            let _ = process_tree::remove_guardian_directory(&guardian_directory);
            Error::new(
                ErrorKind::Process,
                "process.environment",
                format!("cannot construct child PATH: {error}"),
            )
        })?;
        command.env("PATH", path);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::Process,
                    "process.spawn",
                    format!("cannot start pinned Codex app-server guardian: {error}"),
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
                    ErrorKind::Process,
                    "process.spawn",
                    format!("cannot capture guardian process identity: {error}"),
                ));
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::Process,
                    "process.spawn",
                    "guardian stdin was not piped",
                ));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::Process,
                    "process.spawn",
                    "guardian stdout was not piped",
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = process_tree::remove_guardian(&guardian_path);
                let _ = process_tree::remove_guardian_directory(&guardian_directory);
                return Err(Error::new(
                    ErrorKind::Process,
                    "process.spawn",
                    "guardian stderr was not piped",
                ));
            }
        };

        let (outbound_tx, outbound_rx) = mpsc::channel(config.outbound_capacity);
        let (event_tx, event_rx) = mpsc::channel(config.event_capacity);
        let stderr_capture = Arc::new(StderrCapture::new(config.stderr_capacity));
        let inner = Arc::new(ProcessInner {
            outbound: outbound_tx,
            child: Mutex::new(Some(child)),
            identity,
            guardian_path,
            guardian_directory,
            tasks: StdMutex::new(Vec::new()),
            stderr: Arc::clone(&stderr_capture),
            shutdown_started: AtomicBool::new(false),
            cleanup_finished: AtomicBool::new(false),
            shutdown_timeout: config.shutdown_timeout,
            send_timeout: config.request_timeout,
            max_frame_bytes: config.max_frame_bytes,
        });

        let writer = tokio::spawn(writer_loop(
            stdin,
            outbound_rx,
            event_tx.clone(),
            Arc::downgrade(&inner),
        ));
        let reader = tokio::spawn(reader_loop(stdout, event_tx, config.max_frame_bytes));
        let stderr_task = tokio::spawn(stderr_loop(stderr, stderr_capture));
        *inner.tasks() = vec![writer, reader, stderr_task];

        Ok((Self { inner }, event_rx))
    }

    pub(crate) async fn send(&self, value: Value) -> Result<()> {
        self.send_tracked(value, Arc::new(AtomicBool::new(false)))
            .await
    }

    pub(crate) async fn send_tracked(
        &self,
        value: Value,
        dispatched: Arc<AtomicBool>,
    ) -> Result<()> {
        if self.inner.shutdown_started.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorKind::Shutdown,
                "process.send",
                "app-server shutdown has started",
            ));
        }

        let frame = EncodedFrame::encode(&value, self.inner.max_frame_bytes)?;
        let permit = match timeout(self.inner.send_timeout, self.inner.outbound.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return Err(Error::new(
                    ErrorKind::Disconnected,
                    "process.send",
                    "app-server writer task is not available",
                ));
            }
            Err(_) => {
                return Err(Error::timeout(
                    "process.send.queue",
                    self.inner.send_timeout,
                ));
            }
        };
        if self.inner.shutdown_started.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorKind::Shutdown,
                "process.send",
                "app-server shutdown started before dispatch",
            ));
        }

        let (ack_tx, ack_rx) = oneshot::channel();
        dispatched.store(true, Ordering::Release);
        permit.send(Outbound::Frame { frame, ack: ack_tx });

        match timeout(self.inner.send_timeout, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(self
                .outcome_unknown_after_dispatch(format!("app-server stdin write failed: {error}"))
                .await),
            Ok(Err(_)) => Err(self
                .outcome_unknown_after_dispatch(
                    "app-server writer ended before acknowledging the dispatched frame".to_owned(),
                )
                .await),
            Err(_) => Err(self
                .outcome_unknown_after_dispatch(format!(
                    "no write acknowledgement arrived within {} ms",
                    self.inner.send_timeout.as_millis()
                ))
                .await),
        }
    }

    async fn outcome_unknown_after_dispatch(&self, reason: String) -> Error {
        let termination = self.force_kill().await.err();
        let message = termination.map_or_else(
            || format!("{reason}; runtime termination was initiated"),
            |error| format!("{reason}; runtime termination failed: {error}"),
        );
        Error::new(ErrorKind::OutcomeUnknown, "process.send", message)
    }

    pub(crate) fn stderr_tail(&self) -> Option<String> {
        self.inner.stderr.tail()
    }

    pub(crate) async fn stderr_tail_after_close(&self) -> Option<String> {
        self.inner
            .stderr
            .tail_after_close(self.inner.shutdown_timeout.min(Duration::from_secs(1)))
            .await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        if self.inner.cleanup_finished.load(Ordering::Acquire) {
            return Ok(());
        }
        self.inner.shutdown_started.store(true, Ordering::Release);

        let mut failures = Vec::new();
        let (ack_tx, ack_rx) = oneshot::channel();
        match timeout(
            self.inner.shutdown_timeout,
            self.inner.outbound.send(Outbound::Close { ack: ack_tx }),
        )
        .await
        {
            Ok(Ok(())) => match timeout(self.inner.shutdown_timeout, ack_rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => failures.push(error.to_string()),
                Ok(Err(_)) => {
                    failures.push("writer closed before stdin shutdown acknowledgement".to_owned());
                }
                Err(_) => failures.push("timed out while closing app-server stdin".to_owned()),
            },
            Ok(Err(_)) => failures.push("writer task was unavailable during shutdown".to_owned()),
            Err(_) => failures.push("timed out queueing app-server stdin close".to_owned()),
        }

        let mut child_guard = self.inner.child.lock().await;
        if let Some(child) = child_guard.as_mut() {
            match timeout(self.inner.shutdown_timeout, child.wait()).await {
                Ok(Ok(status)) if status.success() => {}
                Ok(Ok(status)) => failures.push(format!(
                    "app-server exited unsuccessfully during shutdown: {status}"
                )),
                Ok(Err(error)) => failures.push(format!("failed waiting for app-server: {error}")),
                Err(_) => {
                    failures.push(
                        "guardian did not exit after stdin was closed; TERM was sent".to_owned(),
                    );
                    if let Err(error) = process_tree::terminate(self.inner.identity, child) {
                        failures.push(format!("failed to terminate owned guardian: {error}"));
                    }
                    match timeout(self.inner.shutdown_timeout, child.wait()).await {
                        Ok(Ok(_status)) => {}
                        Ok(Err(error)) => {
                            failures.push(format!("failed to reap killed app-server: {error}"));
                        }
                        Err(_) => failures.push("timed out reaping killed app-server".to_owned()),
                    }
                }
            }
        }
        *child_guard = None;
        drop(child_guard);

        let tasks = {
            let mut tasks = self.inner.tasks();
            std::mem::take(&mut *tasks)
        };
        for mut task in tasks {
            match timeout(self.inner.shutdown_timeout, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("process task failed: {error}")),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    failures.push("timed out joining process task; task was aborted".to_owned());
                }
            }
        }

        if let Err(error) = process_tree::remove_guardian(&self.inner.guardian_path) {
            failures.push(format!("failed to remove guardian helper: {error}"));
        }
        if let Err(error) = process_tree::remove_guardian_directory(&self.inner.guardian_directory)
        {
            failures.push(format!("failed to remove guardian directory: {error}"));
        }

        self.inner.cleanup_finished.store(true, Ordering::Release);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                Error::new(ErrorKind::Shutdown, "process.shutdown", failures.join("; "))
                    .with_stderr(self.stderr_tail()),
            )
        }
    }

    pub(crate) async fn force_kill(&self) -> Result<()> {
        self.inner.shutdown_started.store(true, Ordering::Release);
        let mut child_guard = self.inner.child.lock().await;
        let Some(child) = child_guard.as_mut() else {
            return Err(Error::new(
                ErrorKind::Process,
                "process.kill",
                "owned guardian child is no longer available",
            ));
        };
        process_tree::terminate(self.inner.identity, child).map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "process.kill",
                format!("failed to terminate owned guardian: {error}"),
            )
        })?;
        match timeout(self.inner.shutdown_timeout, child.wait()).await {
            Ok(Ok(_status)) => {
                *child_guard = None;
                process_tree::remove_guardian(&self.inner.guardian_path).map_err(|error| {
                    Error::new(
                        ErrorKind::Process,
                        "process.kill",
                        format!("failed to remove guardian helper: {error}"),
                    )
                })?;
                process_tree::remove_guardian_directory(&self.inner.guardian_directory).map_err(
                    |error| {
                        Error::new(
                            ErrorKind::Process,
                            "process.kill",
                            format!("failed to remove guardian directory: {error}"),
                        )
                    },
                )?;
                Ok(())
            }
            Ok(Err(error)) => Err(Error::new(
                ErrorKind::Process,
                "process.kill",
                format!("failed to reap terminated app-server: {error}"),
            )),
            Err(_) => Err(Error::new(
                ErrorKind::Process,
                "process.kill",
                "timed out reaping terminated app-server",
            )),
        }
    }

    pub(crate) fn begin_force_shutdown(&self) {
        self.inner.shutdown_started.store(true, Ordering::Release);
        if let Ok(mut child) = self.inner.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = process_tree::terminate(self.inner.identity, child);
        }
    }

    #[cfg(test)]
    pub(crate) async fn with_test_writer<W>(
        writer: W,
        max_frame_bytes: usize,
        send_timeout: Duration,
    ) -> (Self, mpsc::Receiver<ProcessEvent>)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outbound_tx, outbound_rx) = mpsc::channel(4);
        let (event_tx, event_rx) = mpsc::channel(4);
        let stderr = Arc::new(StderrCapture::new(1024));
        let inner = Arc::new(ProcessInner {
            outbound: outbound_tx,
            child: Mutex::new(None),
            identity: process_tree::ProcessIdentity::none(),
            guardian_path: std::path::PathBuf::new(),
            guardian_directory: std::path::PathBuf::new(),
            tasks: StdMutex::new(Vec::new()),
            stderr,
            shutdown_started: AtomicBool::new(false),
            cleanup_finished: AtomicBool::new(false),
            shutdown_timeout: send_timeout,
            send_timeout,
            max_frame_bytes,
        });
        let task = tokio::spawn(writer_loop(
            writer,
            outbound_rx,
            event_tx,
            Arc::downgrade(&inner),
        ));
        inner.tasks().push(task);
        (Self { inner }, event_rx)
    }
}

async fn writer_loop<W>(
    writer: W,
    mut outbound: mpsc::Receiver<Outbound>,
    event_tx: mpsc::Sender<ProcessEvent>,
    inner: Weak<ProcessInner>,
) where
    W: AsyncWrite + Unpin,
{
    let mut writer = JsonLinesWriter::new(writer);
    while let Some(command) = outbound.recv().await {
        match command {
            Outbound::Frame { frame, ack } => {
                let result = writer.write(&frame).await;
                if let Err(error) = result {
                    terminate_after_writer_failure(&inner).await;
                    let _ = ack.send(Err(error.clone()));
                    let _ = event_tx
                        .send(ProcessEvent::Closed(Error::new(
                            ErrorKind::Disconnected,
                            "process.stdin",
                            format!("app-server stdin writer failed: {error}"),
                        )))
                        .await;
                    return;
                }
                let _ = ack.send(Ok(()));
            }
            Outbound::Close { ack } => {
                let _ = ack.send(writer.close().await);
                return;
            }
        }
    }
}

async fn terminate_after_writer_failure(inner: &Weak<ProcessInner>) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    inner.shutdown_started.store(true, Ordering::Release);
    let mut child = inner.child.lock().await;
    if let Some(child) = child.as_mut() {
        let _ = process_tree::terminate(inner.identity, child);
    }
}

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::Sender<ProcessEvent>,
    max_frame_bytes: usize,
) {
    let mut reader = JsonLinesReader::new(stdout, max_frame_bytes);
    loop {
        match reader.next().await {
            Ok(Some(value)) => match wire::parse(value) {
                Ok(message) => {
                    if event_tx.send(ProcessEvent::Message(message)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(ProcessEvent::Closed(error)).await;
                    return;
                }
            },
            Ok(None) => {
                let _ = event_tx
                    .send(ProcessEvent::Closed(Error::new(
                        ErrorKind::Disconnected,
                        "process.stdout",
                        "app-server stdout reached EOF",
                    )))
                    .await;
                return;
            }
            Err(error) => {
                let _ = event_tx.send(ProcessEvent::Closed(error)).await;
                return;
            }
        }
    }
}

async fn stderr_loop(stderr: tokio::process::ChildStderr, capture: Arc<StderrCapture>) {
    capture_stderr(stderr, capture).await;
}

async fn capture_stderr<R>(mut stderr: R, capture: Arc<StderrCapture>)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    let mut pending = Vec::new();
    let mut discarding_oversized_line = false;

    loop {
        let count = match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        pending.extend_from_slice(&buffer[..count]);

        let mut consumed = 0;
        if discarding_oversized_line {
            let Some(position) = pending.iter().position(|byte| *byte == b'\n') else {
                pending.clear();
                continue;
            };
            consumed = position + 1;
            discarding_oversized_line = false;
        }

        while let Some(relative) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
            let position = consumed + relative;
            if position - consumed > MAX_STDERR_LINE_BYTES {
                capture.push(OVERSIZED_STDERR_LINE);
            } else {
                let mut end = position;
                if end > consumed && pending[end - 1] == b'\r' {
                    end -= 1;
                }
                capture.push(&String::from_utf8_lossy(&pending[consumed..end]));
            }
            consumed = position + 1;
        }

        if consumed > 0 {
            let unread = pending.len() - consumed;
            pending.copy_within(consumed.., 0);
            pending.truncate(unread);
        }
        if pending.len() > MAX_STDERR_LINE_BYTES {
            capture.push(OVERSIZED_STDERR_LINE);
            pending.clear();
            discarding_oversized_line = true;
        }
    }

    if !discarding_oversized_line && !pending.is_empty() {
        capture.push(&String::from_utf8_lossy(&pending));
    }
    capture.finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncWrite, AsyncWriteExt as _};

    struct FailAfter {
        remaining: usize,
    }

    impl AsyncWrite for FailAfter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected partial write failure",
                )));
            }
            let count = self.remaining.min(buffer.len());
            self.remaining -= count;
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn oversized_stderr_line_is_omitted_without_leaking_split_secrets() {
        let capture = Arc::new(StderrCapture::new(4 * 1024));
        let (mut writer, reader) = tokio::io::duplex(1024);
        let capture_task = tokio::spawn(capture_stderr(reader, Arc::clone(&capture)));
        let prefix = "x".repeat(MAX_STDERR_LINE_BYTES + 1);
        let writer_task = tokio::spawn(async move {
            writer.write_all(prefix.as_bytes()).await.expect("prefix");
            writer
                .write_all(b"Authorization: Bearer ")
                .await
                .expect("secret marker");
            writer
                .write_all(b"super-secret-token\nsafe line\n")
                .await
                .expect("secret suffix");
        });

        writer_task.await.expect("writer task");
        capture_task.await.expect("capture task");
        let tail = capture.tail().expect("stderr tail");

        assert!(tail.contains(OVERSIZED_STDERR_LINE));
        assert!(tail.contains("safe line"));
        assert!(!tail.contains("super-secret-token"));
        assert!(!tail.contains("Authorization"));
    }

    #[tokio::test]
    async fn predispatch_validation_does_not_set_dispatched_marker() {
        let (process, _events) =
            ProcessHandle::with_test_writer(tokio::io::sink(), 32, Duration::from_secs(1)).await;
        let dispatched = Arc::new(AtomicBool::new(false));
        let error = process
            .send_tracked(json!({"value": "x".repeat(128)}), Arc::clone(&dispatched))
            .await
            .expect_err("oversized frame must fail before dispatch");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert!(!dispatched.load(Ordering::Acquire));
        process.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn partial_write_after_dispatch_is_outcome_unknown_and_closes_transport() {
        let (process, mut events) = ProcessHandle::with_test_writer(
            FailAfter { remaining: 4 },
            1024,
            Duration::from_secs(1),
        )
        .await;
        let dispatched = Arc::new(AtomicBool::new(false));
        let error = process
            .send_tracked(json!({"value": "payload"}), Arc::clone(&dispatched))
            .await
            .expect_err("partial write must fail");
        assert!(dispatched.load(Ordering::Acquire));
        assert_eq!(error.kind(), ErrorKind::OutcomeUnknown);
        match events.recv().await.expect("closed event") {
            ProcessEvent::Closed(error) => assert_eq!(error.kind(), ErrorKind::Disconnected),
            ProcessEvent::Message(_) => panic!("unexpected message"),
        }
    }
}
