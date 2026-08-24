//! Pinned app-server approval request decoding.

use super::{
    ApprovalResponder, CommandAction, CommandApproval, FileChangeApproval, FileSystemAccess,
    FileSystemPermission, FileSystemPermissionPath, FileSystemSpecialPath, NetworkApprovalContext,
    NetworkPolicyAction, NetworkPolicyAmendment, NetworkProtocol, PermissionApproval,
    PermissionGrant, UserInputOption, UserInputQuestion, UserInputRequest,
};
use crate::error::{Error, ErrorKind, Result};
use crate::private::process::ProcessHandle;
use crate::private::wire::RpcId;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn command_approval(
    process: ProcessHandle,
    id: RpcId,
    params: &Value,
    timeout: Duration,
) -> Result<CommandApproval> {
    Ok(CommandApproval {
        thread_id: required_non_empty_string(params, "threadId")?,
        turn_id: required_non_empty_string(params, "turnId")?,
        item_id: required_non_empty_string(params, "itemId")?,
        approval_id: nullable_string(params, "approvalId")?,
        environment_id: nullable_string(params, "environmentId")?,
        started_at_ms: required_i64(params, "startedAtMs")?,
        command: nullable_string(params, "command")?,
        actions: parse_command_actions(params.get("commandActions"))?,
        cwd: nullable_string(params, "cwd")?.map(PathBuf::from),
        reason: nullable_string(params, "reason")?,
        network_context: parse_network_context(params.get("networkApprovalContext"))?,
        proposed_exec_policy_amendment: nullable_string_array(
            params,
            "proposedExecpolicyAmendment",
        )?,
        proposed_network_policy_amendments: parse_network_amendments(
            params.get("proposedNetworkPolicyAmendments"),
        )?,
        responder: ApprovalResponder::new(process, id, json!({"decision": "decline"}), timeout),
    })
}

pub(crate) fn file_approval(
    process: ProcessHandle,
    id: RpcId,
    params: &Value,
    timeout: Duration,
) -> Result<FileChangeApproval> {
    Ok(FileChangeApproval {
        thread_id: required_non_empty_string(params, "threadId")?,
        turn_id: required_non_empty_string(params, "turnId")?,
        item_id: required_non_empty_string(params, "itemId")?,
        started_at_ms: required_i64(params, "startedAtMs")?,
        reason: nullable_string(params, "reason")?,
        grant_root: nullable_string(params, "grantRoot")?.map(PathBuf::from),
        responder: ApprovalResponder::new(process, id, json!({"decision": "decline"}), timeout),
    })
}

pub(crate) fn permission_approval(
    process: ProcessHandle,
    id: RpcId,
    params: &Value,
    timeout: Duration,
) -> Result<PermissionApproval> {
    let permissions = params
        .get("permissions")
        .ok_or_else(|| protocol_field("permissions"))?;
    let requested = parse_permissions(permissions)?;
    Ok(PermissionApproval {
        thread_id: required_non_empty_string(params, "threadId")?,
        turn_id: required_non_empty_string(params, "turnId")?,
        item_id: required_non_empty_string(params, "itemId")?,
        environment_id: nullable_string(params, "environmentId")?,
        started_at_ms: required_i64(params, "startedAtMs")?,
        cwd: PathBuf::from(required_string(params, "cwd")?),
        reason: nullable_string(params, "reason")?,
        requested,
        responder: ApprovalResponder::new(
            process,
            id,
            json!({"permissions": {}, "scope": "turn"}),
            timeout,
        ),
    })
}

pub(crate) fn user_input_request(
    process: ProcessHandle,
    id: RpcId,
    params: &Value,
    timeout: Duration,
) -> Result<UserInputRequest> {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_field("questions"))?
        .iter()
        .map(parse_question)
        .collect::<Result<Vec<_>>>()?;
    let auto_resolution_ms = nullable_u64(params, "autoResolutionMs")?;
    let is_blocking = required_bool(params, "isBlocking")?;
    Ok(UserInputRequest {
        thread_id: required_non_empty_string(params, "threadId")?,
        turn_id: required_non_empty_string(params, "turnId")?,
        item_id: required_non_empty_string(params, "itemId")?,
        auto_resolution_ms,
        is_blocking,
        questions,
        responder: ApprovalResponder::new(process, id, json!({"answers": {}}), timeout),
    })
}

fn parse_question(value: &Value) -> Result<UserInputQuestion> {
    let options = match value.get("options") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|option| {
                Ok(UserInputOption {
                    label: required_string(option, "label")?,
                    description: required_string(option, "description")?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(protocol_field("questions[].options")),
    };
    Ok(UserInputQuestion {
        id: required_non_empty_string(value, "id")?,
        header: required_string(value, "header")?,
        question: required_string(value, "question")?,
        allows_other: optional_bool_with_default(value, "isOther", false)?,
        is_secret: optional_bool_with_default(value, "isSecret", false)?,
        options,
    })
}

fn parse_command_actions(value: Option<&Value>) -> Result<Vec<CommandAction>> {
    let values = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(values)) => values,
        Some(_) => return Err(protocol_field("commandActions")),
    };
    values
        .iter()
        .map(|action| {
            let command = required_string(action, "command")?;
            match action.get("type").and_then(Value::as_str) {
                Some("read") => Ok(CommandAction::Read {
                    command,
                    name: required_string(action, "name")?,
                    path: PathBuf::from(required_string(action, "path")?),
                }),
                Some("listFiles") => Ok(CommandAction::ListFiles {
                    command,
                    path: nullable_string(action, "path")?.map(PathBuf::from),
                }),
                Some("search") => Ok(CommandAction::Search {
                    command,
                    path: nullable_string(action, "path")?.map(PathBuf::from),
                    query: nullable_string(action, "query")?,
                }),
                Some("unknown") => Ok(CommandAction::Unknown { command }),
                Some(other) => Err(Error::new(
                    ErrorKind::Protocol,
                    "approval.parse",
                    format!("unknown command action type '{other}'"),
                )),
                None => Err(protocol_field("commandActions[].type")),
            }
        })
        .collect()
}

fn parse_network_context(value: Option<&Value>) -> Result<Option<NetworkApprovalContext>> {
    let value = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(value @ Value::Object(_)) => value,
        Some(_) => return Err(protocol_field("networkApprovalContext")),
    };
    let protocol = match value.get("protocol").and_then(Value::as_str) {
        Some("http") => NetworkProtocol::Http,
        Some("https") => NetworkProtocol::Https,
        Some("socks5Tcp") => NetworkProtocol::Socks5Tcp,
        Some("socks5Udp") => NetworkProtocol::Socks5Udp,
        Some(other) => {
            return Err(Error::new(
                ErrorKind::Protocol,
                "approval.parse",
                format!("unknown network approval protocol '{other}'"),
            ));
        }
        None => return Err(protocol_field("networkApprovalContext.protocol")),
    };
    Ok(Some(NetworkApprovalContext {
        host: required_non_empty_string(value, "host")?,
        protocol,
    }))
}

fn parse_network_amendments(value: Option<&Value>) -> Result<Vec<NetworkPolicyAmendment>> {
    let values = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(values)) => values,
        Some(_) => return Err(protocol_field("proposedNetworkPolicyAmendments")),
    };
    values
        .iter()
        .map(|amendment| {
            let action = match amendment.get("action").and_then(Value::as_str) {
                Some("allow") => NetworkPolicyAction::Allow,
                Some("deny") => NetworkPolicyAction::Deny,
                Some(other) => {
                    return Err(Error::new(
                        ErrorKind::Protocol,
                        "approval.parse",
                        format!("unknown network policy action '{other}'"),
                    ));
                }
                None => return Err(protocol_field("proposedNetworkPolicyAmendments[].action")),
            };
            Ok(NetworkPolicyAmendment {
                host: required_non_empty_string(amendment, "host")?,
                action,
            })
        })
        .collect()
}

fn parse_permissions(value: &Value) -> Result<PermissionGrant> {
    let source = value
        .as_object()
        .ok_or_else(|| protocol_field("permissions"))?;
    let network = match source.get("network") {
        None | Some(Value::Null) => None,
        Some(Value::Object(network)) => match network.get("enabled") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(enabled)) => Some(*enabled),
            Some(_) => return Err(protocol_field("permissions.network.enabled")),
        },
        Some(_) => return Err(protocol_field("permissions.network")),
    };
    let mut grant = PermissionGrant {
        network,
        ..PermissionGrant::default()
    };
    let file_system = match source.get("fileSystem") {
        None | Some(Value::Null) => return Ok(grant),
        Some(Value::Object(file_system)) => file_system,
        Some(_) => return Err(protocol_field("permissions.fileSystem")),
    };
    grant.entries = parse_path_permissions(
        file_system.get("read"),
        "permissions.fileSystem.read",
        FileSystemAccess::Read,
    )?;
    grant.entries.extend(parse_path_permissions(
        file_system.get("write"),
        "permissions.fileSystem.write",
        FileSystemAccess::Write,
    )?);
    grant.glob_scan_max_depth = match file_system.get("globScanMaxDepth") {
        None | Some(Value::Null) => None,
        Some(Value::Number(value)) => {
            let depth = value
                .as_u64()
                .filter(|depth| *depth > 0)
                .ok_or_else(|| protocol_field("permissions.fileSystem.globScanMaxDepth"))?;
            Some(depth)
        }
        Some(_) => return Err(protocol_field("permissions.fileSystem.globScanMaxDepth")),
    };
    match file_system.get("entries") {
        None | Some(Value::Null) => {}
        Some(Value::Array(entries)) => grant.entries.extend(
            entries
                .iter()
                .map(parse_file_system_permission)
                .collect::<Result<Vec<_>>>()?,
        ),
        Some(_) => return Err(protocol_field("permissions.fileSystem.entries")),
    }
    Ok(grant)
}

fn parse_file_system_permission(value: &Value) -> Result<FileSystemPermission> {
    let access = match value.get("access").and_then(Value::as_str) {
        Some("read") => FileSystemAccess::Read,
        Some("write") => FileSystemAccess::Write,
        Some("deny") => FileSystemAccess::Deny,
        Some(other) => {
            return Err(Error::new(
                ErrorKind::Protocol,
                "approval.parse",
                format!("unknown filesystem access mode '{other}'"),
            ));
        }
        None => return Err(protocol_field("permissions.fileSystem.entries[].access")),
    };
    let path = value
        .get("path")
        .ok_or_else(|| protocol_field("permissions.fileSystem.entries[].path"))?;
    let path = match path.get("type").and_then(Value::as_str) {
        Some("path") => {
            FileSystemPermissionPath::Path(PathBuf::from(required_string(path, "path")?))
        }
        Some("glob_pattern") => {
            FileSystemPermissionPath::GlobPattern(required_string(path, "pattern")?)
        }
        Some("special") => FileSystemPermissionPath::Special(parse_special_path(
            path.get("value")
                .ok_or_else(|| protocol_field("permissions.fileSystem.entries[].path.value"))?,
        )?),
        Some(other) => {
            return Err(Error::new(
                ErrorKind::Protocol,
                "approval.parse",
                format!("unknown filesystem path type '{other}'"),
            ));
        }
        None => return Err(protocol_field("permissions.fileSystem.entries[].path.type")),
    };
    Ok(FileSystemPermission { access, path })
}

fn parse_special_path(value: &Value) -> Result<FileSystemSpecialPath> {
    match value.get("kind").and_then(Value::as_str) {
        Some("root") => Ok(FileSystemSpecialPath::Root),
        Some("minimal") => Ok(FileSystemSpecialPath::Minimal),
        Some("project_roots") => Ok(FileSystemSpecialPath::ProjectRoots {
            subpath: nullable_string(value, "subpath")?.map(PathBuf::from),
        }),
        Some("tmpdir") => Ok(FileSystemSpecialPath::TempDir),
        Some("slash_tmp") => Ok(FileSystemSpecialPath::SlashTmp),
        Some("unknown") => Ok(FileSystemSpecialPath::Unknown {
            path: PathBuf::from(required_string(value, "path")?),
            subpath: nullable_string(value, "subpath")?.map(PathBuf::from),
        }),
        Some(other) => Err(Error::new(
            ErrorKind::Protocol,
            "approval.parse",
            format!("unknown filesystem special path kind '{other}'"),
        )),
        None => Err(protocol_field(
            "permissions.fileSystem.entries[].path.value.kind",
        )),
    }
}

fn parse_path_permissions(
    value: Option<&Value>,
    field: &str,
    access: FileSystemAccess,
) -> Result<Vec<FileSystemPermission>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(|path| FileSystemPermission {
                        access,
                        path: FileSystemPermissionPath::Path(PathBuf::from(path)),
                    })
                    .ok_or_else(|| protocol_field(field))
            })
            .collect(),
        Some(_) => Err(protocol_field(field)),
    }
}

fn nullable_string_array(value: &Value, field: &str) -> Result<Vec<String>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| protocol_field(field))
            })
            .collect(),
        Some(_) => Err(protocol_field(field)),
    }
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| protocol_field(field))
}

fn required_non_empty_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| protocol_field(field))
}

fn required_i64(value: &Value, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| protocol_field(field))
}

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| protocol_field(field))
}

fn nullable_string(value: &Value, field: &str) -> Result<Option<String>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(protocol_field(field)),
    }
}

fn nullable_u64(value: &Value, field: &str) -> Result<Option<u64>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| protocol_field(field)),
        Some(_) => Err(protocol_field(field)),
    }
}

fn optional_bool_with_default(value: &Value, field: &str, default: bool) -> Result<bool> {
    match value.get(field) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(protocol_field(field)),
    }
}

fn protocol_field(field: &str) -> Error {
    Error::new(
        ErrorKind::Protocol,
        "approval.parse",
        format!("missing or invalid field '{field}'"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_permissions_without_losing_authority_boundaries() {
        let parsed = parse_permissions(&json!({
            "network": {"enabled": true},
            "fileSystem": {
                "read": ["/array/read"],
                "write": ["/array/write"],
                "globScanMaxDepth": 4,
                "entries": [
                    {"access": "read", "path": {"type": "path", "path": "/repo/README.md"}},
                    {"access": "write", "path": {"type": "glob_pattern", "pattern": "/repo/src/**"}},
                    {"access": "deny", "path": {"type": "special", "value": {"kind": "slash_tmp"}}}
                ]
            }
        }))
        .expect("permissions");
        assert_eq!(parsed.network, Some(true));
        assert_eq!(parsed.glob_scan_max_depth, Some(4));
        assert_eq!(parsed.entries.len(), 5);
        assert_eq!(parsed.entries[0].access, FileSystemAccess::Read);
        assert!(matches!(
            parsed.entries[0].path,
            FileSystemPermissionPath::Path(ref path) if path == &PathBuf::from("/array/read")
        ));
        assert_eq!(parsed.entries[1].access, FileSystemAccess::Write);
        assert_eq!(parsed.entries[2].access, FileSystemAccess::Read);
        assert!(matches!(
            parsed.entries[4].path,
            FileSystemPermissionPath::Special(FileSystemSpecialPath::SlashTmp)
        ));
    }

    #[test]
    fn rejects_unknown_permission_semantics_instead_of_hiding_them() {
        let error = parse_permissions(&json!({
            "fileSystem": {
                "entries": [{"access": "execute", "path": {"type": "path", "path": "/repo"}}]
            }
        }))
        .expect_err("unknown access must fail");
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn parses_command_network_context_and_actions() {
        let actions = parse_command_actions(Some(&json!([
            {"type": "read", "command": "cat README.md", "name": "README.md", "path": "/repo/README.md"},
            {"type": "search", "command": "rg token", "path": "/repo", "query": "token"}
        ])))
        .expect("actions");
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], CommandAction::Read { .. }));

        let context = parse_network_context(Some(&json!({
            "host": "api.openai.com",
            "protocol": "https"
        })))
        .expect("context")
        .expect("present");
        assert_eq!(context.protocol, NetworkProtocol::Https);
        assert_eq!(context.host, "api.openai.com");
    }

    #[tokio::test]
    async fn preserves_required_is_blocking() {
        async fn parse(value: Option<Value>, id: i64) -> Result<UserInputRequest> {
            let mut params = json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "questions": []
            });
            if let Some(value) = value {
                params["isBlocking"] = value;
            }
            let (process, _events) =
                ProcessHandle::with_test_writer(tokio::io::sink(), 1024, Duration::from_secs(1))
                    .await;
            user_input_request(process, RpcId::Number(id), &params, Duration::from_secs(1))
        }

        let blocking = parse(Some(json!(true)), 1).await.expect("blocking request");
        assert!(blocking.is_blocking());
        drop(blocking);

        let non_blocking = parse(Some(json!(false)), 2)
            .await
            .expect("non-blocking request");
        assert!(!non_blocking.is_blocking());
        drop(non_blocking);

        let missing = parse(None, 3)
            .await
            .expect_err("missing required isBlocking must be rejected");
        assert_eq!(missing.kind(), ErrorKind::Protocol);

        let error = parse(Some(json!("yes")), 4)
            .await
            .expect_err("non-boolean isBlocking must be rejected");
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }
}
