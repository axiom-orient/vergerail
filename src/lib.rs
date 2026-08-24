//! Vergerail is a pinned Rust bridge to OpenAI Codex app-server.
//!
//! The public surface intentionally exposes Codex concepts instead of a speculative
//! cross-provider abstraction. Transport, JSON-RPC, runtime process handles, and
//! authentication material remain private.

mod account;
mod approval;
mod client;
mod config;
mod error;
mod event;
mod model;
mod private;
mod runtime;
mod session;

pub use account::{Account, Login, LoginMethod};
pub use approval::{
    ApprovalEvent, CommandAction, CommandApproval, CommandDecision, FileChangeApproval,
    FileChangeDecision, FileSystemAccess, FileSystemPermission, FileSystemPermissionPath,
    FileSystemSpecialPath, NetworkApprovalContext, NetworkPolicyAction, NetworkPolicyAmendment,
    NetworkProtocol, PermissionApproval, PermissionGrant, UserInputAnswer, UserInputOption,
    UserInputQuestion, UserInputRequest,
};
pub use client::Codex;
pub use config::CodexConfig;
pub use error::{Error, ErrorKind, Result};
pub use event::{
    CommandSummary, Diagnostic, Event, FileChangeSummary, OpaqueEvent, RunResult, TurnAudit,
    TurnStatus, Usage,
};
pub use model::Model;
pub use runtime::{
    DownloadPolicy, ResolvedRuntime, RuntimeOrigin, RuntimePackage, RuntimeResolver,
};
pub use session::{ReasoningEffort, Run, Sandbox, Session, SessionOptions};

#[cfg(test)]
mod contract_tests;
