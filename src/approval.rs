//! Explicit approval and user-input requests sent by Codex.

mod protocol;
mod respond;

pub(crate) use protocol::{
    command_approval, file_approval, permission_approval, user_input_request,
};
use respond::ApprovalResponder;

use crate::error::{Error, ErrorKind, Result};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

/// Decision for a command execution approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandDecision {
    /// Permit only this command request.
    Accept,
    /// Permit this request and equivalent requests for the session.
    AcceptForSession,
    /// Refuse the request and allow the turn to continue.
    Decline,
    /// Cancel the request and the associated work.
    Cancel,
}

/// Decision for a file-change approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileChangeDecision {
    /// Permit only this file change.
    Accept,
    /// Permit this request and equivalent requests for the session.
    AcceptForSession,
    /// Refuse the request and allow the turn to continue.
    Decline,
    /// Cancel the request and the associated work.
    Cancel,
}

/// Protocol reported for a managed-network approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkProtocol {
    /// Plain HTTP.
    Http,
    /// HTTPS.
    Https,
    /// SOCKS5 TCP.
    Socks5Tcp,
    /// SOCKS5 UDP.
    Socks5Udp,
}

/// Exact managed-network target reported by Codex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkApprovalContext {
    /// Host requested by the command.
    pub host: String,
    /// Protocol requested by the command.
    pub protocol: NetworkProtocol,
}

/// Action in a proposed network policy amendment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPolicyAction {
    /// Allow the host in later matching requests.
    Allow,
    /// Deny the host in later matching requests.
    Deny,
}

/// Proposed network policy amendment supplied for caller review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkPolicyAmendment {
    /// Host affected by the amendment.
    pub host: String,
    /// Proposed rule action.
    pub action: NetworkPolicyAction,
}

/// Best-effort command semantics reported by Codex for approval display.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandAction {
    /// Read one named file.
    Read {
        /// Original command text.
        command: String,
        /// Display name reported by Codex.
        name: String,
        /// Absolute path reported by Codex.
        path: PathBuf,
    },
    /// List files, optionally below one path.
    ListFiles {
        /// Original command text.
        command: String,
        /// Optional path argument.
        path: Option<PathBuf>,
    },
    /// Search for text, optionally below one path.
    Search {
        /// Original command text.
        command: String,
        /// Optional path argument.
        path: Option<PathBuf>,
        /// Optional search query.
        query: Option<String>,
    },
    /// Command that Codex could not classify further.
    Unknown {
        /// Original command text.
        command: String,
    },
}

/// Access requested for one structured filesystem entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileSystemAccess {
    /// Read access.
    Read,
    /// Write access.
    Write,
    /// Explicit denial boundary.
    Deny,
}

/// Provider-defined special filesystem root.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileSystemSpecialPath {
    /// Filesystem root.
    Root,
    /// Codex minimal filesystem profile.
    Minimal,
    /// Current project roots, optionally restricted by a relative subpath.
    ProjectRoots {
        /// Optional subpath below each project root.
        subpath: Option<PathBuf>,
    },
    /// Platform temporary directory.
    TempDir,
    /// Literal `/tmp` on Unix-like systems.
    SlashTmp,
    /// Additive provider value not represented by the stable typed variants.
    Unknown {
        /// Provider path label.
        path: PathBuf,
        /// Optional provider subpath.
        subpath: Option<PathBuf>,
    },
}

/// Path selector used by a structured filesystem permission.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileSystemPermissionPath {
    /// Literal path.
    Path(PathBuf),
    /// Glob pattern interpreted by Codex.
    GlobPattern(String),
    /// Provider-defined special root.
    Special(FileSystemSpecialPath),
}

/// One structured filesystem permission requested by Codex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSystemPermission {
    /// Requested access mode.
    pub access: FileSystemAccess,
    /// Requested path selector.
    pub path: FileSystemPermissionPath,
}

/// A caller answer to one Codex user-input question.
#[derive(Clone, Eq, PartialEq)]
pub struct UserInputAnswer {
    /// Question identifier supplied by Codex.
    pub question_id: String,
    /// One or more selected or free-form answers.
    pub answers: Vec<String>,
}

impl fmt::Debug for UserInputAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserInputAnswer")
            .field("question_id", &self.question_id)
            .field("answer_count", &self.answers.len())
            .field("answers", &"[REDACTED]")
            .finish()
    }
}

/// One user-input question supplied by Codex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserInputQuestion {
    /// Stable identifier used when returning an answer.
    pub id: String,
    /// Short display header.
    pub header: String,
    /// Full question text.
    pub question: String,
    /// Whether a free-form answer is accepted.
    pub allows_other: bool,
    /// Whether the answer should be treated as secret by the caller UI.
    pub is_secret: bool,
    /// Optional fixed choices.
    pub options: Vec<UserInputOption>,
}

/// One selectable answer for a user-input question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserInputOption {
    /// User-facing option label.
    pub label: String,
    /// User-facing option description.
    pub description: String,
}

/// Exact typed summary of permissions requested by Codex.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PermissionGrant {
    /// Whether network access was requested.
    pub network: Option<bool>,
    /// Filesystem permissions requested by Codex.
    pub entries: Vec<FileSystemPermission>,
    /// Maximum glob scan depth requested by Codex.
    pub glob_scan_max_depth: Option<u64>,
}

/// A reverse request that must be explicitly resolved.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApprovalEvent {
    /// Request to execute a command.
    Command(CommandApproval),
    /// Request to apply a file change.
    FileChange(FileChangeApproval),
    /// Request for an additional permission profile.
    Permissions(PermissionApproval),
    /// Request for caller-provided input.
    UserInput(UserInputRequest),
}

impl ApprovalEvent {
    /// Resolves any approval event with its fail-closed response.
    pub async fn deny(self) -> Result<()> {
        match self {
            Self::Command(request) => request.respond(CommandDecision::Decline).await,
            Self::FileChange(request) => request.respond(FileChangeDecision::Decline).await,
            Self::Permissions(request) => request.deny().await,
            Self::UserInput(request) => request.answer(Vec::new()).await,
        }
    }
}

/// Command execution approval request.
#[derive(Debug)]
pub struct CommandApproval {
    /// Codex thread identifier.
    pub thread_id: String,
    /// Codex turn identifier.
    pub turn_id: String,
    /// Codex item identifier.
    pub item_id: String,
    /// Opaque approval callback identifier, when supplied.
    pub approval_id: Option<String>,
    /// Environment identifier, when supplied.
    pub environment_id: Option<String>,
    /// Unix timestamp in milliseconds when the approval request started.
    pub started_at_ms: i64,
    /// Command text, when supplied.
    pub command: Option<String>,
    /// Best-effort parsed command actions supplied by Codex.
    pub actions: Vec<CommandAction>,
    /// Command working directory, when supplied.
    pub cwd: Option<PathBuf>,
    /// Human-readable reason, when supplied.
    pub reason: Option<String>,
    /// Managed-network target, when this is a network approval.
    pub network_context: Option<NetworkApprovalContext>,
    /// Proposed command policy amendment for caller display.
    pub proposed_exec_policy_amendment: Vec<String>,
    /// Proposed network policy amendments for caller display.
    pub proposed_network_policy_amendments: Vec<NetworkPolicyAmendment>,
    responder: ApprovalResponder,
}

impl CommandApproval {
    /// Sends exactly one command decision to app-server.
    pub async fn respond(self, decision: CommandDecision) -> Result<()> {
        let decision = match decision {
            CommandDecision::Accept => "accept",
            CommandDecision::AcceptForSession => "acceptForSession",
            CommandDecision::Decline => "decline",
            CommandDecision::Cancel => "cancel",
        };
        self.responder.respond(json!({"decision": decision})).await
    }
}

/// File-change approval request.
#[derive(Debug)]
pub struct FileChangeApproval {
    /// Codex thread identifier.
    pub thread_id: String,
    /// Codex turn identifier.
    pub turn_id: String,
    /// Codex item identifier.
    pub item_id: String,
    /// Unix timestamp in milliseconds when the approval request started.
    pub started_at_ms: i64,
    /// Human-readable reason, when supplied.
    pub reason: Option<String>,
    /// Optional root requested for session-scoped write access.
    pub grant_root: Option<PathBuf>,
    responder: ApprovalResponder,
}

impl FileChangeApproval {
    /// Sends exactly one file-change decision to app-server.
    pub async fn respond(self, decision: FileChangeDecision) -> Result<()> {
        let decision = match decision {
            FileChangeDecision::Accept => "accept",
            FileChangeDecision::AcceptForSession => "acceptForSession",
            FileChangeDecision::Decline => "decline",
            FileChangeDecision::Cancel => "cancel",
        };
        self.responder.respond(json!({"decision": decision})).await
    }
}

/// Additional-permission approval request.
#[derive(Debug)]
pub struct PermissionApproval {
    /// Codex thread identifier.
    pub thread_id: String,
    /// Codex turn identifier.
    pub turn_id: String,
    /// Codex item identifier.
    pub item_id: String,
    /// Environment identifier, when supplied.
    pub environment_id: Option<String>,
    /// Unix timestamp in milliseconds when the approval request started.
    pub started_at_ms: i64,
    /// Directory used by Codex when resolving relative permission entries.
    pub cwd: PathBuf,
    /// Human-readable reason, when supplied.
    pub reason: Option<String>,
    /// Typed summary of the exact requested permission profile.
    pub requested: PermissionGrant,
    responder: ApprovalResponder,
}

impl PermissionApproval {
    /// Denies all requested permissions.
    pub async fn deny(self) -> Result<()> {
        self.responder
            .respond(json!({"permissions": {}, "scope": "turn"}))
            .await
    }
}

/// User-input request emitted during a turn.
#[derive(Debug)]
pub struct UserInputRequest {
    /// Codex thread identifier.
    pub thread_id: String,
    /// Codex turn identifier.
    pub turn_id: String,
    /// Codex item identifier.
    pub item_id: String,
    /// Provider deadline for automatic resolution, when supplied.
    pub auto_resolution_ms: Option<u64>,
    is_blocking: bool,
    /// Questions that require caller input.
    pub questions: Vec<UserInputQuestion>,
    responder: ApprovalResponder,
}

impl UserInputRequest {
    /// Returns whether Codex marks this request as blocking the turn.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        self.is_blocking
    }

    /// Answers the request. An empty vector declines to provide input.
    pub async fn answer(self, answers: Vec<UserInputAnswer>) -> Result<()> {
        let allowed = self
            .questions
            .iter()
            .map(|question| question.id.as_str())
            .collect::<HashSet<_>>();
        let mut observed = HashSet::new();
        let mut output = serde_json::Map::new();
        for answer in answers {
            if answer.question_id.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "user_input.answer",
                    "question id must be non-empty",
                ));
            }
            if !allowed.contains(answer.question_id.as_str()) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "user_input.answer",
                    format!("unknown question id '{}'", answer.question_id),
                ));
            }
            if !observed.insert(answer.question_id.clone()) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "user_input.answer",
                    format!("duplicate answer for question '{}'", answer.question_id),
                ));
            }
            output.insert(answer.question_id, json!({"answers": answer.answers}));
        }
        self.responder
            .respond(json!({
                "answers": Value::Object(output)
            }))
            .await
    }
}
