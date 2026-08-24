use crate::error::{Error, ErrorKind, Result};
use serde_json::{Map, Value};
use std::fmt;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RpcId {
    Number(i64),
    String(String),
}

impl RpcId {
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Self::Number(value) => Value::from(*value),
            Self::String(value) => Value::String(value.clone()),
        }
    }
}

impl fmt::Display for RpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(formatter, "{value}"),
            Self::String(value) => formatter.write_str(value),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Incoming {
    Success {
        id: RpcId,
        result: Value,
    },
    Failure {
        id: RpcId,
        code: i64,
        message: String,
    },
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

pub(crate) fn parse(value: Value) -> Result<Incoming> {
    let object = value.as_object().ok_or_else(|| {
        Error::new(
            ErrorKind::Protocol,
            "rpc.parse",
            "JSON-RPC frame must be an object",
        )
    })?;
    let id = object.get("id").map(parse_id).transpose()?;
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty());
    if object.contains_key("method") && method.is_none() {
        return Err(Error::new(
            ErrorKind::Protocol,
            "rpc.parse",
            "request method must be a non-empty string",
        ));
    }
    if method.is_some() && (object.contains_key("result") || object.contains_key("error")) {
        return Err(Error::new(
            ErrorKind::Protocol,
            "rpc.parse",
            "request and notification frames may not contain result or error",
        ));
    }

    match (id, method) {
        (Some(id), Some(method)) => Ok(Incoming::Request {
            id,
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(method)) => Ok(Incoming::Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(id), None) => parse_response(id, object),
        (None, None) => Err(Error::new(
            ErrorKind::Protocol,
            "rpc.parse",
            "frame is neither a response, request, nor notification",
        )),
    }
}

pub(crate) fn request(id: u64, method: &str, params: Value) -> Value {
    let mut object = Map::with_capacity(3);
    object.insert("id".to_owned(), Value::from(id));
    object.insert("method".to_owned(), Value::String(method.to_owned()));
    object.insert("params".to_owned(), params);
    Value::Object(object)
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    let mut object = Map::with_capacity(2);
    object.insert("method".to_owned(), Value::String(method.to_owned()));
    if !params.is_null() {
        object.insert("params".to_owned(), params);
    }
    Value::Object(object)
}

pub(crate) fn success(id: &RpcId, result: Value) -> Value {
    let mut object = Map::with_capacity(2);
    object.insert("id".to_owned(), id.to_value());
    object.insert("result".to_owned(), result);
    Value::Object(object)
}

pub(crate) fn failure(id: &RpcId, code: i64, message: &str) -> Value {
    let mut error = Map::with_capacity(2);
    error.insert("code".to_owned(), Value::from(code));
    error.insert("message".to_owned(), Value::String(message.to_owned()));

    let mut object = Map::with_capacity(2);
    object.insert("id".to_owned(), id.to_value());
    object.insert("error".to_owned(), Value::Object(error));
    Value::Object(object)
}

fn parse_response(id: RpcId, object: &Map<String, Value>) -> Result<Incoming> {
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(Incoming::Success {
            id,
            result: result.clone(),
        }),
        (None, Some(error)) => {
            let error = error.as_object().ok_or_else(|| {
                Error::new(
                    ErrorKind::Protocol,
                    "rpc.parse",
                    "response error must be an object",
                )
            })?;
            let code = error.get("code").and_then(Value::as_i64).ok_or_else(|| {
                Error::new(
                    ErrorKind::Protocol,
                    "rpc.parse",
                    "response error code must be an integer",
                )
            })?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Protocol,
                        "rpc.parse",
                        "response error message must be a string",
                    )
                })?
                .to_owned();
            Ok(Incoming::Failure { id, code, message })
        }
        _ => Err(Error::new(
            ErrorKind::Protocol,
            "rpc.parse",
            "response must contain exactly one of result or error",
        )),
    }
}

fn parse_id(value: &Value) -> Result<RpcId> {
    if let Some(number) = value.as_i64() {
        return Ok(RpcId::Number(number));
    }
    if let Some(text) = value.as_str() {
        return Ok(RpcId::String(text.to_owned()));
    }
    Err(Error::new(
        ErrorKind::Protocol,
        "rpc.parse",
        "request id must be an integer or string",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bidirectional_request_without_jsonrpc_header() {
        let incoming = parse(json!({
            "id": 7,
            "method": "item/fileChange/requestApproval",
            "params": {"threadId": "t"}
        }))
        .expect("parse");
        match incoming {
            Incoming::Request { id, method, .. } => {
                assert_eq!(id, RpcId::Number(7));
                assert_eq!(method, "item/fileChange/requestApproval");
            }
            _ => panic!("wrong message kind"),
        }
    }

    #[test]
    fn rejects_method_and_result_in_one_frame() {
        let error = parse(json!({"id": 1, "method": "x", "result": {}})).expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn rejects_ambiguous_response() {
        let error = parse(json!({"id": 1, "result": {}, "error": {"code": -1, "message": "x"}}))
            .expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn rejects_empty_or_non_string_method() {
        for frame in [json!({"method": ""}), json!({"method": 1})] {
            let error = parse(frame).expect_err("invalid method must fail");
            assert_eq!(error.kind(), ErrorKind::Protocol);
        }
    }
}
