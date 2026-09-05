//! Minimal JSON-RPC 2.0 types for the Claude Code IDE protocol.
//!
//! The protocol surface is tiny (see `claude-code-ide-protocol-spec.md` §3),
//! so this module deliberately avoids pulling in an MCP SDK: one message per
//! WebSocket text frame, no batches.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// JSON-RPC request id. The CLI uses non-negative integers starting at 0.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Null,
    Num(u64),
    Str(String),
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Null => f.write_str("null"),
            Id::Num(n) => write!(f, "{n}"),
            Id::Str(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Id,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Option<Value>,
}

/// Any inbound frame after parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Request(Request),
    Notification(Notification),
    /// A response to a request *we* sent. The IDE server never sends
    /// requests, so these are logged and dropped.
    Response(Id),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// Outbound response frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: Id, result: Value) -> Self {
        Response {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: Id, error: RpcError) -> Self {
        Response {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Outbound notification frame (IDE → CLI, see PROTO §6).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutgoingNotification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> OutgoingNotification<'a> {
    pub fn new(method: &'a str, params: Value) -> Self {
        OutgoingNotification {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<Id>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug)]
pub enum ParseError {
    Json(serde_json::Error),
    /// Structurally valid JSON that is not a JSON-RPC message.
    Invalid(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Json(e) => write!(f, "invalid JSON: {e}"),
            ParseError::Invalid(why) => write!(f, "invalid JSON-RPC message: {why}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse one text frame into an [`Incoming`] message.
pub fn parse(text: &str) -> Result<Incoming, ParseError> {
    let raw: RawMessage = serde_json::from_str(text).map_err(ParseError::Json)?;
    match (raw.method, raw.id) {
        (Some(method), Some(id)) if id != Id::Null => Ok(Incoming::Request(Request {
            id,
            method,
            params: raw.params,
        })),
        (Some(method), _) => Ok(Incoming::Notification(Notification {
            method,
            params: raw.params,
        })),
        (None, Some(id)) if raw.result.is_some() || raw.error.is_some() => {
            Ok(Incoming::Response(id))
        }
        _ => Err(ParseError::Invalid("missing method")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_request_with_numeric_id() {
        let msg =
            parse(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"a":1}}"#).unwrap();
        assert_eq!(
            msg,
            Incoming::Request(Request {
                id: Id::Num(0),
                method: "initialize".into(),
                params: Some(json!({"a": 1})),
            })
        );
    }

    #[test]
    fn parses_request_with_string_id() {
        let msg = parse(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#).unwrap();
        match msg {
            Incoming::Request(r) => assert_eq!(r.id, Id::Str("abc".into())),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_notification() {
        let msg =
            parse(r#"{"jsonrpc":"2.0","method":"ide_connected","params":{"pid":1}}"#).unwrap();
        assert!(matches!(msg, Incoming::Notification(n) if n.method == "ide_connected"));
    }

    #[test]
    fn null_id_is_a_notification() {
        let msg = parse(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap();
        assert!(matches!(msg, Incoming::Notification(_)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(parse("not json"), Err(ParseError::Json(_))));
        assert!(matches!(parse(r#"{"foo":1}"#), Err(ParseError::Invalid(_))));
    }

    #[test]
    fn response_serialization_omits_absent_fields() {
        let ok = serde_json::to_value(Response::success(Id::Num(1), json!({}))).unwrap();
        assert_eq!(ok, json!({"jsonrpc":"2.0","id":1,"result":{}}));
        let err = serde_json::to_value(Response::failure(
            Id::Num(2),
            RpcError::method_not_found("x"),
        ))
        .unwrap();
        assert_eq!(
            err,
            json!({"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found: x"}})
        );
    }
}
