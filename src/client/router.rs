//! Inbound app-server JSON-RPC routing and protocol-failure handling.

use super::ClientInner;
use crate::approval::{self, ApprovalEvent};
use crate::error::{Error, ErrorKind, Result};
use crate::event::{Event, OpaqueEvent};
use crate::private::process::ProcessEvent;
use crate::private::protocol::{
    command_from_item, compact_notification, config_warning_message, file_change_from_item,
    file_change_from_patch, image_generation_from_item, protocol_field, required_non_empty_string,
    required_string, turn_completion, usage_from_notification,
};
use crate::private::redact::redact_line;
use crate::private::request::ResponseCompletion;
use crate::private::wire::{self, Incoming, RpcId};
use crate::session::{RunEventOutcome, TerminalRouteOutcome};
use serde_json::Value;
use std::sync::{Arc, Weak};
use tokio::sync::mpsc;

impl ClientInner {
    async fn complete_pending(&self, id: RpcId, result: Result<Value>) {
        match self.requests.complete(&id, result) {
            ResponseCompletion::Delivered => {}
            ResponseCompletion::Missing => {
                self.push_diagnostic(
                    "rpc/lateResponse",
                    format!("response for unknown or expired request id {id}"),
                );
            }
            ResponseCompletion::OrphanedSuccess { operation } => {
                self.disconnect(Error::new(
                    ErrorKind::OutcomeUnknown,
                    operation,
                    "a successful non-idempotent response could not be committed to its caller; the runtime was terminated and the request was not retried",
                ))
                .await;
            }
        }
    }

    async fn handle_notification(self: &Arc<Self>, method: String, params: Value) {
        if method == "account/login/completed" {
            self.handle_login_completed(&params).await;
            return;
        }
        if method == "warning" {
            let Some(message) = params.get("message").and_then(Value::as_str) else {
                self.push_diagnostic(
                    "rpc/malformedNotification",
                    "warning notification is missing string field 'message'".to_owned(),
                );
                return;
            };
            if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                self.send_run_event(
                    thread_id,
                    None,
                    &method,
                    Event::Warning(redact_line(message)),
                );
            } else {
                self.push_diagnostic(&method, redact_line(message));
            }
            return;
        }
        if method == "configWarning" {
            match config_warning_message(&params) {
                Ok(message) => self.push_diagnostic(&method, message),
                Err(error) => {
                    self.push_diagnostic("rpc/malformedNotification", error.to_string());
                }
            }
            return;
        }
        if method == "turn/completed" {
            self.finish_from_notification(&params).await;
            return;
        }
        let known_run_notification = matches!(
            method.as_str(),
            "turn/started"
                | "item/agentMessage/delta"
                | "item/commandExecution/outputDelta"
                | "item/fileChange/patchUpdated"
                | "item/started"
                | "item/completed"
                | "thread/tokenUsage/updated"
                | "error"
        );
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let Some(thread_id) = thread_id else {
            if known_run_notification {
                self.disconnect(protocol_field("run.notification", "threadId"))
                    .await;
            } else {
                self.push_diagnostic(&method, compact_notification(&params));
            }
            return;
        };
        let observed_turn_id = if method == "turn/started" {
            params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        } else {
            params
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        };
        if known_run_notification && observed_turn_id.is_none() {
            self.disconnect(protocol_field("run.notification", "turnId"))
                .await;
            return;
        }

        match method.as_str() {
            "turn/started" => {
                self.send_run_event(thread_id, observed_turn_id, &method, Event::Started);
            }
            "item/agentMessage/delta" => {
                let mapped =
                    required_non_empty_string(&params, "itemId", "item.agentMessage.delta")
                        .and_then(|_| required_string(&params, "delta", "item.agentMessage.delta"));
                match mapped {
                    Ok(delta) => {
                        self.send_run_event(
                            thread_id,
                            observed_turn_id,
                            &method,
                            Event::TextDelta(delta),
                        );
                    }
                    Err(error) => self.disconnect(error).await,
                }
            }
            "item/commandExecution/outputDelta" => {
                let mapped = required_non_empty_string(
                    &params,
                    "itemId",
                    "item.commandExecution.outputDelta",
                )
                .and_then(|_| {
                    required_string(&params, "delta", "item.commandExecution.outputDelta")
                });
                match mapped {
                    Ok(delta) => {
                        self.send_run_event(
                            thread_id,
                            observed_turn_id,
                            &method,
                            Event::CommandOutput(delta),
                        );
                    }
                    Err(error) => self.disconnect(error).await,
                }
            }
            "item/fileChange/patchUpdated" => match file_change_from_patch(&params) {
                Ok(summary) => {
                    self.send_run_event(
                        thread_id,
                        observed_turn_id,
                        &method,
                        Event::FileChange(summary),
                    );
                }
                Err(error) => self.disconnect(error).await,
            },
            "item/started" | "item/completed" => {
                let Some(item) = params.get("item") else {
                    self.disconnect(protocol_field("item.lifecycle", "item"))
                        .await;
                    return;
                };
                let Some(item_type) = item.get("type").and_then(Value::as_str) else {
                    self.disconnect(protocol_field("item.lifecycle", "item.type"))
                        .await;
                    return;
                };
                match item_type {
                    "commandExecution" => match command_from_item(item) {
                        Ok(summary) => {
                            self.send_run_event(
                                thread_id,
                                observed_turn_id,
                                &method,
                                Event::Command(summary),
                            );
                        }
                        Err(error) => self.disconnect(error).await,
                    },
                    "fileChange" => match file_change_from_item(item) {
                        Ok(summary) => {
                            self.send_run_event(
                                thread_id,
                                observed_turn_id,
                                &method,
                                Event::FileChange(summary),
                            );
                        }
                        Err(error) => self.disconnect(error).await,
                    },
                    "imageGeneration" => match image_generation_from_item(item) {
                        Ok(image) => {
                            self.send_run_event(
                                thread_id,
                                observed_turn_id,
                                &method,
                                Event::ImageGeneration(image),
                            );
                        }
                        Err(error) => self.disconnect(error).await,
                    },
                    _ => {
                        self.send_run_event(
                            thread_id,
                            observed_turn_id,
                            &method,
                            Event::Unknown(OpaqueEvent {
                                method: method.clone(),
                            }),
                        );
                    }
                }
            }
            "thread/tokenUsage/updated" => match usage_from_notification(&params) {
                Ok(usage) => {
                    self.send_run_event(
                        thread_id,
                        observed_turn_id,
                        &method,
                        Event::UsageUpdated(usage),
                    );
                }
                Err(error) => self.disconnect(error).await,
            },
            "error" => {
                if params.get("willRetry").and_then(Value::as_bool).is_none() {
                    self.disconnect(protocol_field("run.error", "willRetry"))
                        .await;
                    return;
                }
                let Some(message) = params.pointer("/error/message").and_then(Value::as_str) else {
                    self.disconnect(protocol_field("run.error", "error.message"))
                        .await;
                    return;
                };
                self.send_run_event(
                    thread_id,
                    observed_turn_id,
                    &method,
                    Event::Warning(redact_line(message)),
                );
            }
            _ => {
                self.send_run_event(
                    thread_id,
                    observed_turn_id,
                    &method,
                    Event::Unknown(OpaqueEvent {
                        method: method.clone(),
                    }),
                );
            }
        }
    }

    async fn send_reverse_response(
        self: &Arc<Self>,
        response: Value,
        operation: &'static str,
    ) -> bool {
        match self.process.send(response).await {
            Ok(()) => true,
            Err(error) => {
                let kind = if error.kind() == ErrorKind::OutcomeUnknown {
                    ErrorKind::OutcomeUnknown
                } else {
                    ErrorKind::Disconnected
                };
                self.disconnect(Error::new(
                    kind,
                    operation,
                    format!("failed to deliver a mandatory reverse-RPC response: {error}"),
                ))
                .await;
                false
            }
        }
    }

    async fn handle_server_request(self: &Arc<Self>, id: RpcId, method: String, params: Value) {
        let event = match method.as_str() {
            "item/commandExecution/requestApproval" => approval::command_approval(
                self.process.clone(),
                id.clone(),
                &params,
                self.config.approval_timeout,
            )
            .map(ApprovalEvent::Command),
            "item/fileChange/requestApproval" => approval::file_approval(
                self.process.clone(),
                id.clone(),
                &params,
                self.config.approval_timeout,
            )
            .map(ApprovalEvent::FileChange),
            "item/permissions/requestApproval" => approval::permission_approval(
                self.process.clone(),
                id.clone(),
                &params,
                self.config.approval_timeout,
            )
            .map(ApprovalEvent::Permissions),
            "item/tool/requestUserInput" => approval::user_input_request(
                self.process.clone(),
                id.clone(),
                &params,
                self.config.approval_timeout,
            )
            .map(ApprovalEvent::UserInput),
            _ => {
                if !self
                    .send_reverse_response(
                        wire::failure(
                            &id,
                            -32601,
                            "Vergerail does not enable this app-server capability",
                        ),
                        "rpc.unsupportedServerRequest",
                    )
                    .await
                {
                    return;
                }
                self.push_diagnostic(
                    "rpc/unsupportedServerRequest",
                    format!("rejected reverse request '{method}'"),
                );
                return;
            }
        };
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                if !self
                    .send_reverse_response(
                        wire::failure(&id, -32602, error.message()),
                        "rpc.invalidServerRequest",
                    )
                    .await
                {
                    return;
                }
                self.push_diagnostic("rpc/invalidServerRequest", error.to_string());
                return;
            }
        };
        let thread_id = params.get("threadId").and_then(Value::as_str);
        if let Some(thread_id) = thread_id {
            self.send_run_event(
                thread_id,
                params.get("turnId").and_then(Value::as_str),
                &method,
                Event::ApprovalRequested(event),
            );
        } else {
            drop(event);
        }
    }

    fn send_run_event(
        self: &Arc<Self>,
        thread_id: &str,
        observed_turn_id: Option<&str>,
        source_method: &str,
        event: Event,
    ) {
        self.route_run_event(thread_id, observed_turn_id, source_method, event, true);
    }

    pub(super) fn route_run_event(
        self: &Arc<Self>,
        thread_id: &str,
        observed_turn_id: Option<&str>,
        source_method: &str,
        event: Event,
        defer_while_starting: bool,
    ) {
        match self.runs.route_event(
            thread_id,
            observed_turn_id,
            source_method,
            event,
            defer_while_starting,
            self.config.event_capacity,
        ) {
            RunEventOutcome::Unregistered => {
                self.push_diagnostic(
                    "rpc/unroutedNotification",
                    format!("'{source_method}' targeted inactive thread '{thread_id}'"),
                );
            }
            RunEventOutcome::Delivered
            | RunEventOutcome::Deferred
            | RunEventOutcome::IgnoredBeforeTurn
            | RunEventOutcome::IgnoredAfterAbandon
            | RunEventOutcome::IgnoredAfterFailure => {}
            RunEventOutcome::AfterTerminal => {
                self.push_diagnostic(
                    "rpc/lateTurnNotification",
                    format!(
                        "discarded '{source_method}' after the active turn reached a terminal state"
                    ),
                );
            }
            RunEventOutcome::TurnMismatch { expected, observed } => {
                self.push_diagnostic(
                    "rpc/staleTurnNotification",
                    format!(
                        "discarded '{source_method}' for turn '{observed}' while active turn is '{expected}'"
                    ),
                );
            }
            RunEventOutcome::RouteFailure { turn_id, control } => {
                self.interrupt_failed_run(thread_id, &turn_id, control);
            }
        }
    }

    async fn finish_from_notification(self: &Arc<Self>, params: &Value) {
        self.route_terminal_notification(params, true).await;
    }

    pub(super) async fn route_terminal_notification(
        self: &Arc<Self>,
        params: &Value,
        defer_while_starting: bool,
    ) {
        let Some(thread_id) = params
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            self.disconnect(protocol_field("turn.completed", "threadId"))
                .await;
            return;
        };
        let completion = match turn_completion(params) {
            Ok(completion) => completion,
            Err(error) => {
                self.disconnect(error).await;
                return;
            }
        };
        match self.runs.route_terminal(
            thread_id,
            completion,
            params,
            defer_while_starting,
            self.config.event_capacity,
        ) {
            TerminalRouteOutcome::Deferred | TerminalRouteOutcome::Completed => {}
            TerminalRouteOutcome::Unregistered => {
                self.push_diagnostic(
                    "turn/completed",
                    format!("terminal event for unregistered thread '{thread_id}'"),
                );
            }
            TerminalRouteOutcome::Duplicate { turn_id } => {
                self.disconnect(Error::new(
                    ErrorKind::Protocol,
                    "turn.completed",
                    format!(
                        "duplicate terminal event for thread '{thread_id}' and turn '{turn_id}'"
                    ),
                ))
                .await;
            }
            TerminalRouteOutcome::TurnMismatch { expected, observed } => {
                self.push_diagnostic(
                    "rpc/staleTurnNotification",
                    format!(
                        "discarded terminal event for turn '{observed}' while active turn is '{expected}'"
                    ),
                );
            }
        }
    }

    async fn handle_login_completed(&self, params: &Value) {
        let Some(login_id) = params
            .get("loginId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            self.disconnect(protocol_field("account.login.completed", "loginId"))
                .await;
            return;
        };
        let Some(success) = params.get("success").and_then(Value::as_bool) else {
            self.disconnect(protocol_field("account.login.completed", "success"))
                .await;
            return;
        };
        let result = if success {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Authentication,
                "account.login.wait",
                params
                    .get("error")
                    .and_then(Value::as_str)
                    .map(redact_line)
                    .unwrap_or_else(|| "managed ChatGPT login failed".to_owned()),
            ))
        };

        self.complete_login(login_id, result);
    }
}

pub(super) async fn router_loop(
    inner: Weak<ClientInner>,
    mut events: mpsc::Receiver<ProcessEvent>,
) {
    while let Some(event) = events.recv().await {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        match event {
            ProcessEvent::Message(Incoming::Success { id, result }) => {
                inner.complete_pending(id, Ok(result)).await;
            }
            ProcessEvent::Message(Incoming::Failure { id, code, message }) => {
                let operation = inner.requests.operation(&id).unwrap_or("rpc.response");
                inner
                    .complete_pending(id, Err(Error::rpc(operation, code, redact_line(&message))))
                    .await;
            }
            ProcessEvent::Message(Incoming::Notification { method, params }) => {
                inner.handle_notification(method, params).await;
            }
            ProcessEvent::Message(Incoming::Request { id, method, params }) => {
                inner.handle_server_request(id, method, params).await;
            }
            ProcessEvent::Closed(error) => {
                inner.disconnect(error).await;
                return;
            }
        }
    }
}
