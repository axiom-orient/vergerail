//! Fail-closed reverse JSON-RPC response ownership.

use crate::error::{Error, ErrorKind, Result};
use crate::private::process::ProcessHandle;
use crate::private::wire::{self, RpcId};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug)]
pub(super) struct ApprovalResponder {
    commands: Option<oneshot::Sender<ResponseCommand>>,
}

#[derive(Debug)]
enum ResponseCommand {
    Explicit {
        result: Value,
        acknowledgement: oneshot::Sender<Result<()>>,
    },
    Fallback,
}

impl ApprovalResponder {
    pub(super) fn new(
        process: ProcessHandle,
        id: RpcId,
        fallback: Value,
        timeout: Duration,
    ) -> Self {
        let (commands, receiver) = oneshot::channel();
        tokio::spawn(async move {
            approval_response_task(process, id, fallback, timeout, receiver).await;
        });
        Self {
            commands: Some(commands),
        }
    }

    pub(super) async fn respond(mut self, result: Value) -> Result<()> {
        let commands = self.commands.take().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "approval.respond",
                "approval request has already been resolved",
            )
        })?;
        let (acknowledgement, receiver) = oneshot::channel();
        commands
            .send(ResponseCommand::Explicit {
                result,
                acknowledgement,
            })
            .map_err(|_| {
                Error::new(
                    ErrorKind::Disconnected,
                    "approval.respond",
                    "approval response task ended before accepting the decision",
                )
            })?;
        receiver.await.map_err(|_| {
            Error::new(
                ErrorKind::Disconnected,
                "approval.respond",
                "approval response task ended before reporting delivery",
            )
        })?
    }
}

impl Drop for ApprovalResponder {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(ResponseCommand::Fallback);
        }
    }
}

async fn approval_response_task(
    process: ProcessHandle,
    id: RpcId,
    fallback: Value,
    timeout: Duration,
    receiver: oneshot::Receiver<ResponseCommand>,
) {
    let command = tokio::select! {
        command = receiver => command.unwrap_or(ResponseCommand::Fallback),
        () = tokio::time::sleep(timeout) => ResponseCommand::Fallback,
    };
    let (result, acknowledgement) = match command {
        ResponseCommand::Explicit {
            result,
            acknowledgement,
        } => (result, Some(acknowledgement)),
        ResponseCommand::Fallback => (fallback, None),
    };
    let delivery = process.send(wire::success(&id, result)).await;
    if delivery.is_err() {
        let _ = process.force_kill().await;
    }
    if let Some(acknowledgement) = acknowledgement {
        let _ = acknowledgement.send(delivery);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::time::timeout;

    #[tokio::test]
    async fn explicit_response_cancels_the_deadline_and_writes_exactly_once() {
        let (writer, reader) = tokio::io::duplex(1024);
        let (process, _events) =
            ProcessHandle::with_test_writer(writer, 1024, Duration::from_secs(1)).await;
        let responder = ApprovalResponder::new(
            process.clone(),
            RpcId::Number(7),
            json!({"decision": "decline"}),
            Duration::from_millis(40),
        );

        responder
            .respond(json!({"decision": "accept"}))
            .await
            .expect("explicit response");

        let mut lines = BufReader::new(reader).lines();
        let line = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .expect("first response timeout")
            .expect("read")
            .expect("first response");
        assert_eq!(
            serde_json::from_str::<Value>(&line).expect("JSON"),
            json!({"id": 7, "result": {"decision": "accept"}})
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            timeout(Duration::from_millis(30), lines.next_line())
                .await
                .is_err(),
            "deadline emitted a duplicate fallback response"
        );
        process.shutdown().await.expect("shutdown");
    }
}
