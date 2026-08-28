#![forbid(unsafe_code)]

//! Strongly typed, fail-closed Rust ownership of a pinned Codex app-server runtime.
//!
//! Vergerail intentionally exposes a small application-facing surface. Provider-shaped
//! JSON, process ownership, approval protocols, and credential custody stay internal.

mod account;
mod approval;
mod client;
mod config;
mod error;
mod event;
mod image;
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
pub use image::{
    DirectImageRequest, DirectImageResponse, ImageBackground, ImageGeneration,
    ImageGenerationFailure, ImageQuality, ImageSize,
};
pub use model::Model;
pub use runtime::{
    DownloadPolicy, ResolvedRuntime, RuntimeOrigin, RuntimePackage, RuntimeResolver,
};
pub use session::{ReasoningEffort, Run, Sandbox, Session, SessionOptions};

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod contract_tests;
