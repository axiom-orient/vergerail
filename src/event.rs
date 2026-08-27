//! Typed events emitted by a Codex run.

use crate::approval::ApprovalEvent;
use crate::error::{Error, ErrorKind, Result};
use crate::image::ImageGeneration;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

const DIAGNOSTIC_CAPACITY: usize = 128;

/// Terminal status reported by Codex for a turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    /// The turn completed normally.
    Completed,
    /// The turn was interrupted by the caller or runtime.
    Interrupted,
}

/// Token usage for the most recently observed turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    /// Total input tokens.
    pub input_tokens: u64,
    /// Input tokens served from cache.
    pub cached_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Output tokens used for internal reasoning.
    pub reasoning_output_tokens: u64,
    /// Sum reported by Codex.
    pub total_tokens: u64,
    /// Model context window, when reported.
    pub model_context_window: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct TurnCompletion {
    turn_id: String,
    outcome: Result<TurnOutcome>,
}

#[derive(Debug)]
enum TurnOutcome {
    Completed,
    Interrupted,
    Failed(String),
}

impl TurnCompletion {
    pub(crate) fn completed(turn_id: String) -> Self {
        Self {
            turn_id,
            outcome: Ok(TurnOutcome::Completed),
        }
    }

    pub(crate) fn interrupted(turn_id: String) -> Self {
        Self {
            turn_id,
            outcome: Ok(TurnOutcome::Interrupted),
        }
    }

    pub(crate) fn failed(turn_id: String, message: String) -> Self {
        Self {
            turn_id,
            outcome: Ok(TurnOutcome::Failed(message)),
        }
    }

    pub(crate) fn invalid(turn_id: String, error: Error) -> Self {
        Self {
            turn_id,
            outcome: Err(error),
        }
    }

    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub(crate) fn into_result(
        self,
        thread_id: &str,
        text: String,
        usage: Option<Usage>,
    ) -> Result<RunResult> {
        let status = match self.outcome? {
            TurnOutcome::Completed => TurnStatus::Completed,
            TurnOutcome::Interrupted => TurnStatus::Interrupted,
            TurnOutcome::Failed(message) => {
                return Err(Error::new(ErrorKind::Rpc, "turn.completed", message));
            }
        };
        Ok(RunResult {
            thread_id: thread_id.to_owned(),
            turn_id: self.turn_id,
            text,
            status,
            usage,
        })
    }
}

/// Final observable result of a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunResult {
    /// Codex thread identifier.
    pub thread_id: String,
    /// Codex turn identifier.
    pub turn_id: String,
    /// Concatenated assistant text deltas.
    pub text: String,
    /// Terminal turn status.
    pub status: TurnStatus,
    /// Most recent token usage, when reported.
    pub usage: Option<Usage>,
}

/// Minimal command information suitable for display and auditing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSummary {
    /// Codex item identifier.
    pub item_id: String,
    /// Command text reported by Codex.
    pub command: String,
    /// Working directory reported by Codex.
    pub cwd: Option<PathBuf>,
    /// Current provider-defined status string.
    pub status: String,
}

/// Minimal proposed file-change information.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChangeSummary {
    /// Codex item identifier.
    pub item_id: String,
    /// Paths affected by the proposed patch.
    pub paths: Vec<PathBuf>,
    /// Current provider-defined status string.
    pub status: String,
}

/// Persisted typed evidence for one exact completed turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnAudit {
    /// Codex turn identifier requested by the caller.
    pub turn_id: String,
    /// Persisted command items in provider order.
    pub commands: Vec<CommandSummary>,
    /// Persisted file-change items in provider order.
    pub file_changes: Vec<FileChangeSummary>,
    /// Persisted image-generation items in provider order.
    pub image_generations: Vec<ImageGeneration>,
    /// Types of all remaining persisted items, preserving occurrence and order.
    pub other_item_types: Vec<String>,
}

/// Diagnostic for a valid but unsupported additive notification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueEvent {
    /// Original app-server method name.
    pub method: String,
}

/// Bounded diagnostic captured outside a specific run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    /// Original app-server method name.
    pub method: String,
    /// Redacted message or compact description.
    pub message: String,
}

/// Thread-safe bounded ownership for diagnostics outside a specific run.
pub(crate) struct DiagnosticBuffer {
    items: Mutex<VecDeque<Diagnostic>>,
}

impl DiagnosticBuffer {
    pub(crate) fn new() -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
        }
    }

    fn items(&self) -> MutexGuard<'_, VecDeque<Diagnostic>> {
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn take(&self) -> Vec<Diagnostic> {
        self.items().drain(..).collect()
    }

    pub(crate) fn push(&self, diagnostic: Diagnostic) {
        let mut items = self.items();
        if items.len() >= DIAGNOSTIC_CAPACITY {
            while items.len() > DIAGNOSTIC_CAPACITY.saturating_sub(2) {
                items.pop_front();
            }
            if !items
                .iter()
                .any(|item| item.method == "vergerail/diagnosticsOverflow")
            {
                items.push_back(Diagnostic {
                    method: "vergerail/diagnosticsOverflow".to_owned(),
                    message: "older diagnostics were discarded after reaching the bounded capacity"
                        .to_owned(),
                });
            }
        }
        while items.len() >= DIAGNOSTIC_CAPACITY {
            items.pop_front();
        }
        items.push_back(diagnostic);
    }
}

/// One event from a running Codex turn.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// The turn was accepted by app-server.
    Started,
    /// Incremental assistant text.
    TextDelta(String),
    /// A command item started or changed state.
    Command(CommandSummary),
    /// Incremental command stdout/stderr.
    CommandOutput(String),
    /// A file-change item started or changed state.
    FileChange(FileChangeSummary),
    /// An image-generation item started or changed state.
    ImageGeneration(ImageGeneration),
    /// Codex requires an explicit caller decision or answer.
    ApprovalRequested(ApprovalEvent),
    /// Updated usage information.
    UsageUpdated(Usage),
    /// Non-terminal warning emitted by app-server.
    Warning(String),
    /// Additive notification not interpreted by this pinned adapter.
    Unknown(OpaqueEvent),
    /// The turn completed or was interrupted.
    Completed(RunResult),
    /// The turn failed.
    Failed(Error),
}

#[cfg(test)]
mod diagnostic_buffer_tests {
    use super::{DIAGNOSTIC_CAPACITY, Diagnostic, DiagnosticBuffer};

    #[test]
    fn overflow_is_bounded_and_reported_once() {
        let buffer = DiagnosticBuffer::new();
        for index in 0..=DIAGNOSTIC_CAPACITY {
            buffer.push(Diagnostic {
                method: format!("method-{index}"),
                message: format!("message-{index}"),
            });
        }

        let diagnostics = buffer.take();
        assert_eq!(diagnostics.len(), DIAGNOSTIC_CAPACITY);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|item| item.method == "vergerail/diagnosticsOverflow")
                .count(),
            1
        );
        assert_eq!(
            diagnostics.last().map(|item| item.method.as_str()),
            Some("method-128")
        );
        assert!(buffer.take().is_empty());
    }
}
