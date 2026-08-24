//! ChatGPT account and managed login types.

use crate::client::ClientInner;
use crate::error::{Error, ErrorKind, Result};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::oneshot;

const EARLY_LOGIN_COMPLETION_CAPACITY: usize = 32;

/// Owned state for all managed login handles associated with one client.
pub(crate) struct LoginRegistry {
    state: Mutex<LoginState>,
}

struct LoginState {
    next_waiter_id: u64,
    active: HashMap<String, LoginEntry>,
    early_completed: HashMap<String, Result<()>>,
    early_order: VecDeque<String>,
}

impl Default for LoginState {
    fn default() -> Self {
        Self {
            next_waiter_id: 1,
            active: HashMap::new(),
            early_completed: HashMap::new(),
            early_order: VecDeque::new(),
        }
    }
}

struct LoginEntry {
    terminal: Option<Result<()>>,
    waiters: HashMap<u64, oneshot::Sender<Result<()>>>,
}

pub(crate) enum LoginWait {
    Completed(Result<()>),
    Pending {
        waiter_id: u64,
        receiver: oneshot::Receiver<Result<()>>,
    },
}

impl LoginRegistry {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(LoginState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, LoginState> {
        // The lock protects only in-memory login bookkeeping and never spans an
        // await. Recovering poison preserves terminal delivery for live handles.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn register(&self, login_id: &str) -> bool {
        let mut state = self.state();
        let LoginState {
            active,
            early_completed,
            early_order,
            ..
        } = &mut *state;
        match active.entry(login_id.to_owned()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                let terminal = early_completed.remove(login_id);
                if terminal.is_some() {
                    early_order.retain(|candidate| candidate != login_id);
                }
                entry.insert(LoginEntry {
                    terminal,
                    waiters: HashMap::new(),
                });
                true
            }
        }
    }

    pub(crate) fn wait(&self, login_id: &str) -> Result<LoginWait> {
        let mut state = self.state();
        let LoginState {
            next_waiter_id,
            active,
            ..
        } = &mut *state;
        let entry = active.get_mut(login_id).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "account.login.wait",
                "login handle is no longer active",
            )
        })?;
        if let Some(result) = &entry.terminal {
            return Ok(LoginWait::Completed(result.clone()));
        }

        let waiter_id = *next_waiter_id;
        *next_waiter_id = waiter_id.checked_add(1).ok_or_else(|| {
            Error::new(
                ErrorKind::Protocol,
                "account.login.wait",
                "login waiter id space exhausted",
            )
        })?;
        let (sender, receiver) = oneshot::channel();
        entry.waiters.insert(waiter_id, sender);
        Ok(LoginWait::Pending {
            waiter_id,
            receiver,
        })
    }

    pub(crate) fn remove_waiter(&self, login_id: &str, waiter_id: u64) {
        let mut state = self.state();
        if let Some(entry) = state.active.get_mut(login_id) {
            entry.waiters.remove(&waiter_id);
        }
    }

    pub(crate) fn release(&self, login_id: &str) {
        let mut state = self.state();
        state.active.remove(login_id);
        state.early_completed.remove(login_id);
        state.early_order.retain(|candidate| candidate != login_id);
    }

    pub(crate) fn complete(&self, login_id: &str, result: Result<()>) {
        let waiters = {
            let mut state = self.state();
            let Some(entry) = state.active.get_mut(login_id) else {
                cache_early_login_result(&mut state, login_id, result);
                return;
            };
            if entry.terminal.is_some() {
                return;
            }
            entry.terminal = Some(result.clone());
            std::mem::take(&mut entry.waiters)
        };
        for waiter in waiters.into_values() {
            let _ = waiter.send(result.clone());
        }
    }

    pub(crate) fn fail_active(&self, error: &Error) {
        let waiters = {
            let mut state = self.state();
            let mut waiters = Vec::new();
            for entry in state.active.values_mut() {
                if entry.terminal.is_none() {
                    entry.terminal = Some(Err(error.clone()));
                }
                waiters.extend(std::mem::take(&mut entry.waiters).into_values());
            }
            waiters
        };
        for waiter in waiters {
            let _ = waiter.send(Err(error.clone()));
        }
    }
}

fn cache_early_login_result(state: &mut LoginState, login_id: &str, result: Result<()>) {
    if state.early_completed.contains_key(login_id) {
        return;
    }
    if state.early_completed.len() >= EARLY_LOGIN_COMPLETION_CAPACITY
        && let Some(expired) = state.early_order.pop_front()
    {
        state.early_completed.remove(&expired);
    }
    state.early_order.push_back(login_id.to_owned());
    state.early_completed.insert(login_id.to_owned(), result);
}

/// Account state reported by Codex app-server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Account {
    /// No account is currently authenticated in the dedicated Codex home.
    SignedOut {
        /// Whether app-server requires OpenAI authentication for model use.
        requires_openai_auth: bool,
    },
    /// A ChatGPT account authenticated and managed by Codex.
    ChatGpt {
        /// Account email, when supplied by Codex.
        email: Option<String>,
        /// Provider-defined subscription plan name.
        plan: String,
    },
}

/// Supported managed ChatGPT login mechanisms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginMethod {
    /// Browser-based OAuth login.
    Browser,
    /// Device-code login suitable for terminals and headless hosts.
    DeviceCode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LoginFlow {
    Browser {
        auth_url: String,
    },
    DeviceCode {
        verification_url: String,
        user_code: String,
    },
}

/// In-progress managed ChatGPT login.
pub struct Login {
    inner: Arc<ClientInner>,
    login_id: String,
    flow: LoginFlow,
}

impl Login {
    pub(crate) fn browser(inner: Arc<ClientInner>, login_id: String, auth_url: String) -> Self {
        Self {
            inner,
            login_id,
            flow: LoginFlow::Browser { auth_url },
        }
    }

    pub(crate) fn device_code(
        inner: Arc<ClientInner>,
        login_id: String,
        verification_url: String,
        user_code: String,
    ) -> Self {
        Self {
            inner,
            login_id,
            flow: LoginFlow::DeviceCode {
                verification_url,
                user_code,
            },
        }
    }

    /// Returns the app-server login operation identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.login_id
    }

    /// Returns the browser authorization URL for browser login.
    #[must_use]
    pub fn auth_url(&self) -> Option<&str> {
        match &self.flow {
            LoginFlow::Browser { auth_url } => Some(auth_url),
            LoginFlow::DeviceCode { .. } => None,
        }
    }

    /// Returns the verification URL for device-code login.
    #[must_use]
    pub fn verification_url(&self) -> Option<&str> {
        match &self.flow {
            LoginFlow::Browser { .. } => None,
            LoginFlow::DeviceCode {
                verification_url, ..
            } => Some(verification_url),
        }
    }

    /// Returns the one-time user code for device-code login.
    #[must_use]
    pub fn user_code(&self) -> Option<&str> {
        match &self.flow {
            LoginFlow::Browser { .. } => None,
            LoginFlow::DeviceCode { user_code, .. } => Some(user_code),
        }
    }

    /// Waits for the matching login completion notification and returns the account.
    pub async fn wait(&self) -> Result<Account> {
        self.inner.wait_login(&self.login_id).await?;
        self.inner.account().await
    }

    /// Cancels this login operation.
    ///
    /// The handle remains usable so a caller can inspect the terminal login result.
    pub async fn cancel(&self) -> Result<()> {
        self.inner.cancel_login(&self.login_id).await
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.inner.release_login(&self.login_id);
    }
}

impl fmt::Debug for Login {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let method = match self.flow {
            LoginFlow::Browser { .. } => "browser",
            LoginFlow::DeviceCode { .. } => "device-code",
        };
        formatter
            .debug_struct("Login")
            .field("login_id", &self.login_id)
            .field("method", &method)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn early_completion_is_promoted_when_handle_registers() {
        let registry = LoginRegistry::new();
        registry.complete("login-1", Ok(()));
        assert!(registry.register("login-1"));
        match registry.wait("login-1").expect("wait") {
            LoginWait::Completed(result) => assert_eq!(result, Ok(())),
            LoginWait::Pending { .. } => panic!("early completion was not promoted"),
        }
    }

    #[test]
    fn first_terminal_result_wins_for_a_live_handle() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        registry.complete("login-1", Ok(()));
        registry.complete(
            "login-1",
            Err(Error::new(
                ErrorKind::Authentication,
                "account.login.wait",
                "late cancellation",
            )),
        );
        match registry.wait("login-1").expect("wait") {
            LoginWait::Completed(result) => assert_eq!(result, Ok(())),
            LoginWait::Pending { .. } => panic!("terminal result was not retained"),
        }
    }

    #[test]
    fn release_removes_the_login_entry() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        registry.complete("login-1", Ok(()));
        registry.release("login-1");

        let error = match registry.wait("login-1") {
            Ok(_) => panic!("released login remained waitable"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn completion_delivers_all_waiters_and_retains_the_terminal_result() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));

        let first = match registry.wait("login-1").expect("first waiter") {
            LoginWait::Pending { receiver, .. } => receiver,
            LoginWait::Completed(_) => panic!("login completed before notification"),
        };
        let second = match registry.wait("login-1").expect("second waiter") {
            LoginWait::Pending { receiver, .. } => receiver,
            LoginWait::Completed(_) => panic!("login completed before notification"),
        };

        registry.complete("login-1", Ok(()));
        assert_eq!(first.await.expect("first delivery"), Ok(()));
        assert_eq!(second.await.expect("second delivery"), Ok(()));
        match registry.wait("login-1").expect("terminal wait") {
            LoginWait::Completed(result) => assert_eq!(result, Ok(())),
            LoginWait::Pending { .. } => panic!("terminal result was not retained"),
        }
    }

    #[tokio::test]
    async fn disconnect_fails_pending_waiters_and_retains_the_failure() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        let receiver = match registry.wait("login-1").expect("waiter") {
            LoginWait::Pending { receiver, .. } => receiver,
            LoginWait::Completed(_) => panic!("login completed before disconnect"),
        };
        let disconnect = Error::new(
            ErrorKind::Disconnected,
            "client.disconnect",
            "runtime disconnected",
        );

        registry.fail_active(&disconnect);
        assert_eq!(
            receiver.await.expect("disconnect delivery"),
            Err(disconnect.clone())
        );
        match registry.wait("login-1").expect("terminal wait") {
            LoginWait::Completed(result) => assert_eq!(result, Err(disconnect)),
            LoginWait::Pending { .. } => panic!("disconnect failure was not retained"),
        }
    }

    #[test]
    fn disconnect_does_not_overwrite_an_existing_terminal_result() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        registry.complete("login-1", Ok(()));
        registry.fail_active(&Error::new(
            ErrorKind::Disconnected,
            "client.disconnect",
            "late disconnect",
        ));

        match registry.wait("login-1").expect("terminal wait") {
            LoginWait::Completed(result) => assert_eq!(result, Ok(())),
            LoginWait::Pending { .. } => panic!("terminal result was overwritten"),
        }
    }

    #[test]
    fn completed_wait_does_not_consume_a_waiter_id() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        registry.complete("login-1", Ok(()));
        registry.state().next_waiter_id = u64::MAX;

        match registry.wait("login-1").expect("completed wait") {
            LoginWait::Completed(result) => assert_eq!(result, Ok(())),
            LoginWait::Pending { .. } => panic!("completed login allocated a waiter"),
        }
        assert_eq!(registry.state().next_waiter_id, u64::MAX);
    }

    #[test]
    fn waiter_id_exhaustion_does_not_wrap() {
        let registry = LoginRegistry::new();
        assert!(registry.register("login-1"));
        registry.state().next_waiter_id = u64::MAX;

        let error = match registry.wait("login-1") {
            Ok(_) => panic!("waiter id space wrapped after exhaustion"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert_eq!(registry.state().next_waiter_id, u64::MAX);
    }
}
