//! Codex app-server protocol-to-domain mapping helpers.
//!
//! This module owns provider-shaped JSON interpretation. The client layer owns
//! request lifecycle, session state, and event routing after these values have
//! been reduced to the public domain types.

use crate::error::{Error, ErrorKind, Result};
use crate::event::{CommandSummary, FileChangeSummary, TurnAudit, TurnCompletion, Usage};
use crate::image::{ImageGeneration, ImageGenerationFailure};
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
    let details = optional_string(params, "details", "config.warning")?;
    let path = optional_string(params, "path", "config.warning")?;
    let mut message = summary.to_owned();
    if let Some(details) = details.as_deref().filter(|value| !value.is_empty()) {
        message.push_str(": ");
        message.push_str(details);
    }
    if let Some(path) = path.as_deref().filter(|value| !value.is_empty()) {
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

pub(crate) fn image_generation_from_item(item: &Value) -> Result<ImageGeneration> {
    const OPERATION: &str = "item.imageGeneration";
    let saved_path = optional_string(item, "savedPath", OPERATION)?.map(PathBuf::from);
    if saved_path.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(protocol_field(OPERATION, "savedPath"));
    }

    Ok(ImageGeneration::new(
        required_non_empty_string(item, "id", OPERATION)?,
        required_non_empty_string(item, "status", OPERATION)?,
        optional_string(item, "revisedPrompt", OPERATION)?,
        required_string(item, "result", OPERATION)?,
        optional_bool(item, "transparentBackground", OPERATION)?,
        image_generation_failure(item.get("failure"))?,
        saved_path,
    ))
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
        image_generations: Vec::new(),
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
            "imageGeneration" => audit
                .image_generations
                .push(image_generation_from_item(item)?),
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

fn optional_string(value: &Value, field: &str, operation: &'static str) -> Result<Option<String>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(protocol_field(operation, field)),
    }
}

fn optional_bool(value: &Value, field: &str, operation: &'static str) -> Result<Option<bool>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(protocol_field(operation, field)),
    }
}

fn image_generation_failure(value: Option<&Value>) -> Result<Option<ImageGenerationFailure>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    const OPERATION: &str = "item.imageGeneration";
    match value.get("type").and_then(Value::as_str) {
        Some("usageLimitExceeded") => {
            let limit_id = required_non_empty_string(value, "limitId", OPERATION)?;
            let resets_at = match value.get("resetsAt") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_i64()
                        .ok_or_else(|| protocol_field(OPERATION, "failure.resetsAt"))?,
                ),
            };
            Ok(Some(ImageGenerationFailure::UsageLimitExceeded {
                limit_id,
                resets_at,
            }))
        }
        Some(other) => Err(Error::new(
            ErrorKind::Protocol,
            OPERATION,
            format!("unexpected image-generation failure type '{other}'"),
        )),
        None => Err(protocol_field(OPERATION, "failure.type")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnStatus;
    use serde_json::json;

    #[test]
    fn command_and_file_items_require_pinned_typed_fields() {
        let command = command_from_item(&json!({
            "type": "commandExecution",
            "id": "command-1",
            "command": "pwd",
            "cwd": "/tmp/project",
            "status": "completed"
        }))
        .expect("valid command item");
        assert_eq!(command.item_id, "command-1");
        assert_eq!(command.cwd, Some(PathBuf::from("/tmp/project")));

        let error = command_from_item(&json!({
            "type": "commandExecution",
            "id": "command-1",
            "command": "pwd",
            "cwd": "/tmp/project"
        }))
        .expect_err("missing status must fail");
        assert_eq!(error.kind(), ErrorKind::Protocol);

        let file = file_change_from_item(&json!({
            "type": "fileChange",
            "id": "patch-1",
            "status": "completed",
            "changes": [{"path": "src/lib.rs"}, {"path": "README.md"}]
        }))
        .expect("valid file-change item");
        assert_eq!(file.paths.len(), 2);

        let error = file_change_from_patch(&json!({
            "itemId": "patch-1",
            "changes": [{"kind": "update"}]
        }))
        .expect_err("missing change path must fail");
        assert!(error.message().contains("changes[0].path"));
    }

    #[test]
    fn image_generation_maps_completed_and_usage_limit_items() {
        let completed = image_generation_from_item(&json!({
            "type": "imageGeneration",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "a minimal black square",
            "result": "aW1hZ2U=",
            "transparentBackground": true,
            "failure": null,
            "savedPath": "/tmp/generated_images/image-1.png"
        }))
        .expect("completed image item");
        assert_eq!(completed.id(), "image-1");
        assert_eq!(completed.status(), "completed");
        assert_eq!(completed.result_base64(), "aW1hZ2U=");
        assert_eq!(completed.transparent_background(), Some(true));
        assert_eq!(
            completed.saved_path(),
            Some(std::path::Path::new("/tmp/generated_images/image-1.png"))
        );

        let failed = image_generation_from_item(&json!({
            "type": "imageGeneration",
            "id": "image-2",
            "status": "failed",
            "revisedPrompt": "prompt",
            "result": "",
            "failure": {
                "type": "usageLimitExceeded",
                "limitId": "images",
                "resetsAt": 1234
            }
        }))
        .expect("typed usage-limit failure");
        assert_eq!(
            failed.failure(),
            Some(&ImageGenerationFailure::UsageLimitExceeded {
                limit_id: "images".to_owned(),
                resets_at: Some(1234),
            })
        );
    }

    #[test]
    fn image_generation_rejects_malformed_optional_fields_and_paths() {
        for item in [
            json!({
                "id": "image-1", "status": "completed", "result": "x",
                "savedPath": "relative.png"
            }),
            json!({
                "id": "image-1", "status": "completed", "result": "x",
                "transparentBackground": "yes"
            }),
            json!({
                "id": "image-1", "status": "failed", "result": "",
                "failure": {"type": "futureFailure"}
            }),
        ] {
            let error = image_generation_from_item(&item).expect_err("malformed image item");
            assert_eq!(error.kind(), ErrorKind::Protocol);
            assert_eq!(error.operation(), "item.imageGeneration");
        }
    }

    #[test]
    fn turn_audit_extracts_typed_evidence_and_preserves_other_order() {
        let audit = turn_audit(
            &json!({
                "thread": {
                    "id": "thread-1",
                    "turns": [{
                        "id": "turn-1",
                        "status": "completed",
                        "itemsView": "full",
                        "items": [
                            {"type": "agentMessage", "id": "agent-1"},
                            {"type": "commandExecution", "id": "command-1",
                             "command": "pwd", "cwd": "/tmp/project", "status": "completed"},
                            {"type": "imageGeneration", "id": "image-1", "status": "completed",
                             "result": "aW1hZ2U=", "savedPath": "/tmp/image.png"},
                            {"type": "fileChange", "id": "patch-1", "status": "completed",
                             "changes": [{"path": "README.md"}]},
                            {"type": "reasoning", "id": "reasoning-1"}
                        ]
                    }]
                }
            }),
            "thread-1",
            "turn-1",
        )
        .expect("full audit");

        assert_eq!(audit.commands.len(), 1);
        assert_eq!(audit.file_changes.len(), 1);
        assert_eq!(audit.image_generations.len(), 1);
        assert_eq!(audit.image_generations[0].id(), "image-1");
        assert_eq!(audit.other_item_types, ["agentMessage", "reasoning"]);
    }

    #[test]
    fn turn_audit_rejects_wrong_duplicate_partial_and_invalid_item_identity() {
        let wrong_thread = turn_audit(
            &json!({"thread": {"id": "other", "turns": []}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("wrong thread");
        assert!(wrong_thread.message().contains("other"));

        let duplicate = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [
                {"id": "turn-1", "status": "completed", "items": []},
                {"id": "turn-1", "status": "completed", "items": []}
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

        let duplicate_item = turn_audit(
            &json!({"thread": {"id": "thread-1", "turns": [{
                "id": "turn-1", "status": "completed", "items": [
                    {"type": "agentMessage", "id": "item-1"},
                    {"type": "agentMessage", "id": "item-1"}
                ]
            }]}}),
            "thread-1",
            "turn-1",
        )
        .expect_err("duplicate item id");
        assert!(duplicate_item.message().contains("duplicate item id"));
    }

    #[test]
    fn turn_audit_requires_completed_target() {
        for status in [
            None,
            Some("inProgress"),
            Some("interrupted"),
            Some("failed"),
        ] {
            let mut turn = json!({"id": "turn-1", "items": []});
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
        }
    }

    #[test]
    fn terminal_mapping_is_typed_and_redacts_failures() {
        let completed = turn_completion(&json!({
            "turn": {"id": "turn-1", "status": "completed"}
        }))
        .expect("completed turn");
        assert_eq!(
            completed
                .into_result("thread-1", "done".to_owned(), None)
                .expect("result")
                .status,
            TurnStatus::Completed
        );

        let failed = turn_completion(&json!({
            "turn": {
                "id": "turn-2",
                "status": "failed",
                "error": {"message": "Authorization: Bearer secret-token"}
            }
        }))
        .expect("failed envelope")
        .into_result("thread-1", String::new(), None)
        .expect_err("failed result");
        assert_eq!(failed.kind(), ErrorKind::Rpc);
        assert!(!failed.message().contains("secret-token"));

        let unknown = turn_completion(&json!({
            "turn": {"id": "turn-3", "status": "inProgress"}
        }))
        .expect("terminal envelope")
        .into_result("thread-1", String::new(), None)
        .expect_err("unexpected status");
        assert_eq!(unknown.kind(), ErrorKind::Protocol);
    }
}
