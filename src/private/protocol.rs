//! Codex app-server protocol-to-domain mapping helpers.
//!
//! This module owns provider-shaped JSON interpretation. The client layer owns
//! request lifecycle, session state, and event routing after these values have
//! been reduced to the public domain types.

use crate::error::{Error, ErrorKind, Result};
use crate::event::{CommandSummary, FileChangeSummary, TurnAudit, TurnCompletion, Usage};
use crate::private::redact::redact_line;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) fn turn_completion(params: &Value) -> Result<TurnCompletion> {
    let turn = params
        .get("turn")
        .ok_or_else(|| protocol_field("turn.completed", "turn"))?;
    let turn_id = turn
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| protocol_field("turn.completed", "turn.id"))?;
    Ok(match turn.get("status").and_then(Value::as_str) {
        Some("completed") => TurnCompletion::completed(turn_id),
        Some("interrupted") => TurnCompletion::interrupted(turn_id),
        Some("failed") => TurnCompletion::failed(
            turn_id,
            turn.pointer("/error/message")
                .and_then(Value::as_str)
                .map(redact_line)
                .unwrap_or_else(|| "Codex turn failed".to_owned()),
        ),
        Some(other) => TurnCompletion::invalid(
            turn_id,
            Error::new(
                ErrorKind::Protocol,
                "turn.completed",
                format!("unexpected terminal status '{other}'"),
            ),
        ),
        None => TurnCompletion::invalid(turn_id, protocol_field("turn.completed", "turn.status")),
    })
}

pub(crate) fn config_warning_message(params: &Value) -> Result<String> {
    let summary = params
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_field("config.warning", "summary"))?;
    let details = match params.get("details") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(protocol_field("config.warning", "details")),
    };
    let path = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(protocol_field("config.warning", "path")),
    };
    let mut message = summary.to_owned();
    if let Some(details) = details.filter(|value| !value.is_empty()) {
        message.push_str(": ");
        message.push_str(details);
    }
    if let Some(path) = path.filter(|value| !value.is_empty()) {
        message.push_str(" [");
        message.push_str(path);
        message.push(']');
    }
    Ok(redact_line(&message))
}

pub(crate) fn validate_unsubscribe_response(response: &Value) -> Result<()> {
    match response.get("status").and_then(Value::as_str) {
        Some("notLoaded" | "notSubscribed" | "unsubscribed") => Ok(()),
        Some(other) => Err(Error::new(
            ErrorKind::Protocol,
            "thread.unsubscribe",
            format!("unexpected unsubscribe status '{other}'"),
        )),
        None => Err(protocol_field("thread.unsubscribe", "status")),
    }
}

pub(crate) fn required_string(
    value: &Value,
    field: &str,
    operation: &'static str,
) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| protocol_field(operation, field))
}

pub(crate) fn required_non_empty_string(
    value: &Value,
    field: &str,
    operation: &'static str,
) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| protocol_field(operation, field))
}

pub(crate) fn protocol_field(operation: &'static str, field: &str) -> Error {
    Error::new(
        ErrorKind::Protocol,
        operation,
        format!("missing or invalid field '{field}'"),
    )
}

pub(crate) fn compact_notification(params: &Value) -> String {
    params
        .get("message")
        .and_then(Value::as_str)
        .map(redact_line)
        .unwrap_or_else(|| "notification captured without exposing raw provider payload".to_owned())
}

pub(crate) fn usage_from_notification(params: &Value) -> Result<Usage> {
    let usage = params
        .get("tokenUsage")
        .ok_or_else(|| protocol_field("token_usage.updated", "tokenUsage"))?;
    let last = usage
        .get("last")
        .ok_or_else(|| protocol_field("token_usage.updated", "tokenUsage.last"))?;
    let required_u64 = |field: &str| {
        last.get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| protocol_field("token_usage.updated", field))
    };
    let model_context_window = match usage.get("modelContextWindow") {
        Some(Value::Null) | None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            protocol_field("token_usage.updated", "tokenUsage.modelContextWindow")
        })?),
    };
    Ok(Usage {
        input_tokens: required_u64("inputTokens")?,
        cached_input_tokens: required_u64("cachedInputTokens")?,
        output_tokens: required_u64("outputTokens")?,
        reasoning_output_tokens: required_u64("reasoningOutputTokens")?,
        total_tokens: required_u64("totalTokens")?,
        model_context_window,
    })
}

pub(crate) fn command_from_item(item: &Value) -> Result<CommandSummary> {
    Ok(CommandSummary {
        item_id: required_non_empty_string(item, "id", "item.commandExecution")?,
        command: required_non_empty_string(item, "command", "item.commandExecution")?,
        cwd: Some(PathBuf::from(required_non_empty_string(
            item,
            "cwd",
            "item.commandExecution",
        )?)),
        status: required_non_empty_string(item, "status", "item.commandExecution")?,
    })
}

pub(crate) fn file_change_from_item(item: &Value) -> Result<FileChangeSummary> {
    Ok(FileChangeSummary {
        item_id: required_non_empty_string(item, "id", "item.fileChange")?,
        paths: file_change_paths(item, "item.fileChange")?,
        status: required_non_empty_string(item, "status", "item.fileChange")?,
    })
}

pub(crate) fn file_change_from_patch(params: &Value) -> Result<FileChangeSummary> {
    Ok(FileChangeSummary {
        item_id: required_non_empty_string(params, "itemId", "item.fileChange.patchUpdated")?,
        paths: file_change_paths(params, "item.fileChange.patchUpdated")?,
        status: "inProgress".to_owned(),
    })
}

pub(crate) fn turn_audit(
    response: &Value,
    expected_thread_id: &str,
    target_turn_id: &str,
) -> Result<TurnAudit> {
    let thread = response
        .get("thread")
        .ok_or_else(|| protocol_field("thread.read", "thread"))?;
    let observed_thread_id = required_non_empty_string(thread, "id", "thread.read")?;
    if observed_thread_id != expected_thread_id {
        return Err(Error::new(
            ErrorKind::Protocol,
            "thread.read",
            format!(
                "app-server returned thread '{observed_thread_id}' instead of requested thread '{expected_thread_id}'"
            ),
        ));
    }
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_field("thread.read", "thread.turns"))?;
    let mut target = None;
    for (index, turn) in turns.iter().enumerate() {
        let turn_id = turn
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| protocol_field("thread.read", &format!("thread.turns[{index}].id")))?;
        if turn_id == target_turn_id && target.replace((index, turn)).is_some() {
            return Err(Error::new(
                ErrorKind::Protocol,
                "thread.read",
                format!("thread history contains duplicate target turn '{target_turn_id}'"),
            ));
        }
    }
    let (turn_index, turn) = target.ok_or_else(|| {
        Error::new(
            ErrorKind::Protocol,
            "thread.read",
            format!("thread history does not contain target turn '{target_turn_id}'"),
        )
    })?;
    match turn.get("status").and_then(Value::as_str) {
        Some("completed") => {}
        Some(status) => {
            return Err(Error::new(
                ErrorKind::Protocol,
                "thread.read",
                format!("target turn '{target_turn_id}' is not completed (status '{status}')"),
            ));
        }
        None => {
            return Err(protocol_field(
                "thread.read",
                &format!("thread.turns[{turn_index}].status"),
            ));
        }
    }
    match turn.get("itemsView") {
        None => {}
        Some(Value::String(value)) if value == "full" => {}
        Some(Value::String(value)) => {
            return Err(Error::new(
                ErrorKind::Protocol,
                "thread.read",
                format!("target turn '{target_turn_id}' returned partial items view '{value}'"),
            ));
        }
        Some(_) => {
            return Err(protocol_field(
                "thread.read",
                &format!("thread.turns[{turn_index}].itemsView"),
            ));
        }
    }
    let items = turn.get("items").and_then(Value::as_array).ok_or_else(|| {
        protocol_field("thread.read", &format!("thread.turns[{turn_index}].items"))
    })?;
    let mut audit = TurnAudit {
        turn_id: target_turn_id.to_owned(),
        commands: Vec::new(),
        file_changes: Vec::new(),
        other_item_types: Vec::new(),
    };
    let mut item_ids = HashSet::new();
    for (item_index, item) in items.iter().enumerate() {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                protocol_field(
                    "thread.read",
                    &format!("thread.turns[{turn_index}].items[{item_index}].type"),
                )
            })?;
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                protocol_field(
                    "thread.read",
                    &format!("thread.turns[{turn_index}].items[{item_index}].id"),
                )
            })?;
        if !item_ids.insert(item_id) {
            return Err(Error::new(
                ErrorKind::Protocol,
                "thread.read",
                format!("target turn '{target_turn_id}' contains duplicate item id '{item_id}'"),
            ));
        }
        match item_type {
            "commandExecution" => audit.commands.push(command_from_item(item)?),
            "fileChange" => audit.file_changes.push(file_change_from_item(item)?),
            other => audit.other_item_types.push(other.to_owned()),
        }
    }
    Ok(audit)
}

fn file_change_paths(value: &Value, operation: &'static str) -> Result<Vec<PathBuf>> {
    let changes = value
        .get("changes")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_field(operation, "changes"))?;
    changes
        .iter()
        .enumerate()
        .map(|(index, change)| {
            change
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| protocol_field(operation, &format!("changes[{index}].path")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnStatus;
    use serde_json::json;

    #[test]
    fn command_items_require_the_pinned_typed_fields() {
        let summary = command_from_item(&json!({
            "type": "commandExecution",
            "id": "command-1",
            "command": "pwd",
            "cwd": "/tmp/project",
            "status": "completed"
        }))
        .expect("valid command item");

        assert_eq!(summary.item_id, "command-1");
        assert_eq!(summary.command, "pwd");
        assert_eq!(summary.cwd, Some(PathBuf::from("/tmp/project")));
        assert_eq!(summary.status, "completed");

        let error = command_from_item(&json!({
            "type": "commandExecution",
            "id": "command-1",
            "command": "pwd",
            "cwd": "/tmp/project"
        }))
        .expect_err("missing status must not be silently defaulted");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert_eq!(error.operation(), "item.commandExecution");

        let error = command_from_item(&json!({
            "type": "commandExecution",
            "id": "",
            "command": "pwd",
            "cwd": "/tmp/project",
            "status": "completed"
        }))
        .expect_err("empty item identity must not be accepted");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert!(error.message().contains("id"));
    }

    #[test]
    fn file_change_items_reject_malformed_changes_instead_of_dropping_them() {
        let summary = file_change_from_item(&json!({
            "type": "fileChange",
            "id": "patch-1",
            "status": "completed",
            "changes": [
                {"path": "src/lib.rs"},
                {"path": "README.md"}
            ]
        }))
        .expect("valid file change item");

        assert_eq!(summary.item_id, "patch-1");
        assert_eq!(
            summary.paths,
            vec![PathBuf::from("src/lib.rs"), PathBuf::from("README.md")]
        );
        assert_eq!(summary.status, "completed");

        let error = file_change_from_patch(&json!({
            "itemId": "patch-1",
            "changes": [{"kind": "update"}]
        }))
        .expect_err("missing path must not be silently dropped");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert_eq!(error.operation(), "item.fileChange.patchUpdated");
        assert!(error.message().contains("changes[0].path"));
    }

    #[test]
    fn turn_audit_extracts_only_typed_evidence_and_preserves_other_item_order() {
        let audit = turn_audit(
            &json!({
                "thread": {
                    "id": "thread-1",
                    "turns": [{
                        "id": "turn-1",
                        "status": "completed",
                        "items": [
                            {"type": "agentMessage", "id": "agent-1"},
                            {"type": "commandExecution", "id": "command-1",
                             "command": "pwd", "cwd": "/tmp/project", "status": "failed"},
                            {"type": "reasoning", "id": "reasoning-1"},
                            {"type": "fileChange", "id": "patch-1", "status": "completed",
                             "changes": [{"path": "src/lib.rs"}]},
                            {"type": "agentMessage", "id": "agent-2"}
                        ]
                    }]
                }
            }),
            "thread-1",
            "turn-1",
        )
        .expect("full audit with default itemsView");

        assert_eq!(audit.turn_id, "turn-1");
        assert_eq!(audit.commands.len(), 1);
        assert_eq!(audit.commands[0].item_id, "command-1");
        assert_eq!(audit.file_changes.len(), 1);
        assert_eq!(audit.file_changes[0].item_id, "patch-1");
        assert_eq!(
            audit.other_item_types,
            ["agentMessage", "reasoning", "agentMessage"]
        );
    }

    #[test]
    fn turn_audit_rejects_wrong_duplicate_or_partial_history() {
        let wrong_thread = turn_audit(
            &json!({"thread": {"id": "thread-other", "turns": []}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("wrong thread identity");
        assert_eq!(wrong_thread.kind(), ErrorKind::Protocol);
        assert!(wrong_thread.message().contains("thread-other"));

        let duplicate = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [
                {"id": "turn-1", "status": "completed", "itemsView": "full", "items": []},
                {"id": "turn-1", "status": "completed", "itemsView": "full", "items": []}
            ]}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("duplicate target turn");
        assert!(duplicate.message().contains("duplicate"));

        let partial = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [
                {"id": "turn-1", "status": "completed", "itemsView": "summary", "items": []}
            ]}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("partial history");
        assert!(partial.message().contains("partial"));
    }

    #[test]
    fn turn_audit_requires_one_target_and_validates_every_item_identity() {
        let missing = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [
                {"id": "turn-other", "status": "completed", "itemsView": "full", "items": []}
            ]}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("missing target turn");
        assert!(missing.message().contains("does not contain"));

        let malformed = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [{
                "id": "turn-1", "status": "completed", "itemsView": "full", "items": [
                    {"type": "agentMessage", "id": ""}
                ]
            }]}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("empty item id");
        assert_eq!(malformed.kind(), ErrorKind::Protocol);
        assert!(malformed.message().contains("items[0].id"));

        let duplicate_item = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [{
                "id": "turn-1", "status": "completed", "itemsView": "full", "items": [
                    {"type": "agentMessage", "id": "item-1"},
                    {"type": "commandExecution", "id": "item-1",
                     "command": "pwd", "cwd": "/tmp", "status": "completed"}
                ]
            }]}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("duplicate durable item identity");
        assert_eq!(duplicate_item.kind(), ErrorKind::Protocol);
        assert!(duplicate_item.message().contains("duplicate item id"));
    }

    #[test]
    fn turn_audit_rejects_missing_or_non_completed_status() {
        for (status, expected) in [
            (None, "status"),
            (Some("inProgress"), "not completed"),
            (Some("interrupted"), "not completed"),
            (Some("failed"), "not completed"),
        ] {
            let mut turn = json!({
                "id": "turn-1",
                "itemsView": "full",
                "items": []
            });
            if let Some(status) = status {
                turn["status"] = json!(status);
            }
            let error = turn_audit(
                &json!({"thread": {"id": "thread-1", "turns": [turn]}}),
                "thread-1",
                "turn-1",
            )
            .expect_err("only completed turns are auditable");
            assert_eq!(error.kind(), ErrorKind::Protocol);
            assert!(error.message().contains(expected), "{error}");
        }
    }

    #[test]
    fn maps_terminal_turns_without_exposing_provider_json_to_the_router() {
        let completed = turn_completion(&json!({
            "turn": {"id": "turn-1", "status": "completed"}
        }))
        .expect("completed turn");
        assert_eq!(completed.turn_id(), "turn-1");
        assert_eq!(
            completed
                .into_result("thread-1", "done".to_owned(), None)
                .expect("completed result")
                .status,
            TurnStatus::Completed
        );

        let interrupted = turn_completion(&json!({
            "turn": {"id": "turn-2", "status": "interrupted"}
        }))
        .expect("interrupted turn");
        assert_eq!(
            interrupted
                .into_result("thread-1", String::new(), None)
                .expect("interrupted result")
                .status,
            TurnStatus::Interrupted
        );
    }

    #[test]
    fn maps_failed_turns_to_redacted_rpc_errors() {
        let failed = turn_completion(&json!({
            "turn": {
                "id": "turn-1",
                "status": "failed",
                "error": {"message": "Authorization: Bearer secret-token"}
            }
        }))
        .expect("failed turn envelope");
        let error = failed
            .into_result("thread-1", String::new(), None)
            .expect_err("failed result");

        assert_eq!(error.kind(), ErrorKind::Rpc);
        assert_eq!(error.operation(), "turn.completed");
        assert!(!error.message().contains("secret-token"));
    }

    #[test]
    fn rejects_unknown_terminal_status() {
        let completion = turn_completion(&json!({
            "turn": {"id": "turn-1", "status": "inProgress"}
        }))
        .expect("terminal envelope");
        let error = completion
            .into_result("thread-1", String::new(), None)
            .expect_err("non-terminal status");

        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert_eq!(error.operation(), "turn.completed");
    }
}
