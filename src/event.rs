//! Typed events emitted by a Codex run.

use crate::approval::ApprovalEvent;
use crate::error::{Error, ErrorKind, Result};
use crate::image::ImageGeneration;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

const DIAGNOSTIC_CAPACITY: usize = 128;
const DIAGNOSTIC_ITEM_BYTES: usize = 8 * 1024;
const DIAGNOSTIC_TOTAL_BYTES: usize = 1024 * 1024;

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
        image_generations: Vec<ImageGeneration>,
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
            image_generations,
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
    /// Latest lifecycle state for each generated image, in first-seen order.
    pub image_generations: Vec<ImageGeneration>,
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
    bytes: Mutex<usize>,
}

impl DiagnosticBuffer {
    pub(crate) fn new() -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            bytes: Mutex::new(0),
        }
    }

    fn items(&self) -> MutexGuard<'_, VecDeque<Diagnostic>> {
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn take(&self) -> Vec<Diagnostic> {
        let items = self.items().drain(..).collect();
        *self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = 0;
        items
    }

    pub(crate) fn push(&self, diagnostic: Diagnostic) {
        let diagnostic = bound_diagnostic(diagnostic);
        let mut items = self.items();
        let mut bytes = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let diagnostic_size = diagnostic_bytes(&diagnostic);
        let overflow = Diagnostic {
            method: "vergerail/diagnosticsOverflow".to_owned(),
            message:
                "older diagnostics were discarded after reaching a bounded count or byte limit"
                    .to_owned(),
        };
        let overflow_bytes = diagnostic_bytes(&overflow);
        if items.len() >= DIAGNOSTIC_CAPACITY
            || bytes.saturating_add(diagnostic_size) > DIAGNOSTIC_TOTAL_BYTES
        {
            while (items.len() >= DIAGNOSTIC_CAPACITY.saturating_sub(1)
                || bytes
                    .saturating_add(diagnostic_size)
                    .saturating_add(if has_overflow(&items) {
                        0
                    } else {
                        overflow_bytes
                    })
                    > DIAGNOSTIC_TOTAL_BYTES)
                && !items.is_empty()
            {
                if let Some(removed) = items.pop_front() {
                    *bytes = bytes.saturating_sub(diagnostic_bytes(&removed));
                }
            }
            if !has_overflow(&items) && items.len() < DIAGNOSTIC_CAPACITY {
                *bytes = bytes.saturating_add(overflow_bytes);
                items.push_back(overflow);
            }
        }
        while (items.len() >= DIAGNOSTIC_CAPACITY
            || bytes.saturating_add(diagnostic_size) > DIAGNOSTIC_TOTAL_BYTES)
            && !items.is_empty()
        {
            if let Some(removed) = items.pop_front() {
                *bytes = bytes.saturating_sub(diagnostic_bytes(&removed));
            }
        }
        *bytes = bytes.saturating_add(diagnostic_size);
        items.push_back(diagnostic);
    }
}

fn diagnostic_bytes(diagnostic: &Diagnostic) -> usize {
    diagnostic
        .method
        .len()
        .saturating_add(diagnostic.message.len())
}

fn has_overflow(items: &VecDeque<Diagnostic>) -> bool {
    items
        .iter()
        .any(|item| item.method == "vergerail/diagnosticsOverflow")
}

fn bound_diagnostic(mut diagnostic: Diagnostic) -> Diagnostic {
    let mut method_limit = diagnostic.method.len().min(DIAGNOSTIC_ITEM_BYTES);
    while method_limit > 0 && !diagnostic.method.is_char_boundary(method_limit) {
        method_limit -= 1;
    }
    diagnostic.method.truncate(method_limit);
    let message_limit = DIAGNOSTIC_ITEM_BYTES.saturating_sub(diagnostic.method.len());
    if diagnostic.message.len() > message_limit {
        let mut end = message_limit;
        while end > 0 && !diagnostic.message.is_char_boundary(end) {
            end -= 1;
        }
        diagnostic.message.truncate(end);
    }
    diagnostic
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
    use super::{
        DIAGNOSTIC_CAPACITY, DIAGNOSTIC_ITEM_BYTES, DIAGNOSTIC_TOTAL_BYTES, Diagnostic,
        DiagnosticBuffer, diagnostic_bytes,
    };

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

    #[test]
    fn item_limit_truncates_method_and_message_at_utf8_boundaries() {
        let buffer = DiagnosticBuffer::new();
        buffer.push(Diagnostic {
            method: "진단".repeat(DIAGNOSTIC_ITEM_BYTES),
            message: "메시지".repeat(DIAGNOSTIC_ITEM_BYTES),
        });

        let diagnostics = buffer.take();
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert!(diagnostic.method.is_char_boundary(diagnostic.method.len()));
        assert!(
            diagnostic
                .message
                .is_char_boundary(diagnostic.message.len())
        );
        assert!(diagnostic_bytes(diagnostic) <= DIAGNOSTIC_ITEM_BYTES);
    }

    #[test]
    fn aggregate_limit_keeps_byte_accounting_bounded_and_resets_on_take() {
        let buffer = DiagnosticBuffer::new();
        let message = "x".repeat(DIAGNOSTIC_ITEM_BYTES);
        for index in 0..DIAGNOSTIC_CAPACITY {
            buffer.push(Diagnostic {
                method: format!("method-{index}"),
                message: message.clone(),
            });
        }
        buffer.push(Diagnostic {
            method: "after-limit".to_owned(),
            message: "new".to_owned(),
        });

        let diagnostics = buffer.take();
        assert!(diagnostics.len() <= DIAGNOSTIC_CAPACITY);
        assert!(diagnostics.iter().map(diagnostic_bytes).sum::<usize>() <= DIAGNOSTIC_TOTAL_BYTES);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|item| item.method == "vergerail/diagnosticsOverflow")
                .count(),
            1
        );

        buffer.push(Diagnostic {
            method: "after-reset".to_owned(),
            message: "ok".to_owned(),
        });
        assert_eq!(buffer.take().len(), 1);
    }
}
