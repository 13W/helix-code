//! JSON-RPC 2.0 types for the Agent Client Protocol.
//!
//! Adapted from helix-lsp/src/jsonrpc.rs with ACP-specific error codes added.

use serde::de::{self, DeserializeOwned, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC error codes, extended with ACP-specific values.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    // ACP-specific
    AuthenticationRequired,
    ResourceNotFound,
    ServerError(i64),
}

impl ErrorCode {
    pub fn code(&self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::AuthenticationRequired => -32000,
            ErrorCode::ResourceNotFound => -32002,
            ErrorCode::ServerError(code) => *code,
        }
    }
}

impl From<i64> for ErrorCode {
    fn from(code: i64) -> Self {
        match code {
            -32700 => ErrorCode::ParseError,
            -32600 => ErrorCode::InvalidRequest,
            -32601 => ErrorCode::MethodNotFound,
            -32602 => ErrorCode::InvalidParams,
            -32603 => ErrorCode::InternalError,
            -32000 => ErrorCode::AuthenticationRequired,
            -32002 => ErrorCode::ResourceNotFound,
            code => ErrorCode::ServerError(code),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let code: i64 = Deserialize::deserialize(deserializer)?;
        Ok(ErrorCode::from(code))
    }
}

impl Serialize for ErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i64(self.code())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Error {
            code: ErrorCode::InvalidParams,
            message: message.into(),
            data: None,
        }
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Error {
            code: ErrorCode::InternalError,
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// JSON-RPC request ID.
#[derive(Debug, PartialEq, Eq, Clone, Hash, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Id {
    Null,
    Num(#[serde(deserialize_with = "deserialize_id_num")] u64),
    Str(String),
}

fn deserialize_id_num<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let num = serde_json::Number::deserialize(deserializer)?;

    if let Some(val) = num.as_u64() {
        return Ok(val);
    }

    // Accept floats representing whole positive numbers (some JS implementations send these).
    if let Some(val) = num
        .as_f64()
        .filter(|f| f.is_sign_positive() && f.fract() == 0.0)
    {
        return Ok(val as u64);
    }

    Err(de::Error::custom(
        "id must be a non-negative integer or float representing a whole number",
    ))
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Null => f.write_str("null"),
            Id::Num(n) => write!(f, "{}", n),
            Id::Str(s) => f.write_str(s),
        }
    }
}

/// JSON-RPC protocol version.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Version {
    V2,
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

struct VersionVisitor;

impl Visitor<'_> for VersionVisitor {
    type Value = Version;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("the string \"2.0\"")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        match value {
            "2.0" => Ok(Version::V2),
            _ => Err(de::Error::custom("invalid JSON-RPC version")),
        }
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_identifier(VersionVisitor)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Params {
    None,
    Array(Vec<Value>),
    Map(serde_json::Map<String, Value>),
}

impl Params {
    pub fn parse<D: DeserializeOwned>(self) -> Result<D, Error> {
        let value: Value = self.into();
        serde_json::from_value(value)
            .map_err(|err| Error::invalid_params(format!("invalid params: {err}")))
    }

    pub fn is_none(&self) -> bool {
        self == &Params::None
    }
}

impl From<Params> for Value {
    fn from(params: Params) -> Value {
        match params {
            Params::Array(vec) => Value::Array(vec),
            Params::Map(map) => Value::Object(map),
            Params::None => Value::Null,
        }
    }
}

fn default_params() -> Params {
    Params::None
}

fn default_id() -> Id {
    Id::Null
}

/// An incoming request from the agent to the client (or client to agent).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MethodCall {
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default = "default_params", skip_serializing_if = "Params::is_none")]
    pub params: Params,
    pub id: Id,
}

/// A one-way notification (no response expected).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Notification {
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default = "default_params", skip_serializing_if = "Params::is_none")]
    pub params: Params,
}

/// Any JSON-RPC call (request or notification).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Call {
    MethodCall(MethodCall),
    Notification(Notification),
    Invalid {
        #[serde(default = "default_id")]
        id: Id,
    },
}

impl From<MethodCall> for Call {
    fn from(mc: MethodCall) -> Self {
        Call::MethodCall(mc)
    }
}

impl From<Notification> for Call {
    fn from(n: Notification) -> Self {
        Call::Notification(n)
    }
}

// Response types

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Success {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub result: Value,
    pub id: Id,
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
pub struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub error: Error,
    pub id: Id,
}

/// A response to a request (success or failure).
/// Failure is listed first so that a message containing both `result` and `error` is a Failure.
#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Output {
    Failure(Failure),
    Success(Success),
}

impl From<Output> for Result<Value, Error> {
    fn from(output: Output) -> Self {
        match output {
            Output::Success(s) => Ok(s.result),
            Output::Failure(f) => Err(f.error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_call_round_trip() {
        let m = MethodCall {
            jsonrpc: Some(Version::V2),
            method: "session/prompt".to_owned(),
            params: Params::Map({
                let mut map = serde_json::Map::new();
                map.insert("sessionId".to_owned(), Value::String("s1".to_owned()));
                map
            }),
            id: Id::Num(1),
        };
        let s = serde_json::to_string(&m).unwrap();
        let m2: MethodCall = serde_json::from_str(&s).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn notification_no_params() {
        let n = Notification {
            jsonrpc: Some(Version::V2),
            method: "session/cancel".to_owned(),
            params: Params::None,
        };
        let s = serde_json::to_string(&n).unwrap();
        assert_eq!(s, r#"{"jsonrpc":"2.0","method":"session/cancel"}"#);
    }

    #[test]
    fn acp_error_codes() {
        assert_eq!(ErrorCode::AuthenticationRequired.code(), -32000);
        assert_eq!(ErrorCode::ResourceNotFound.code(), -32002);
        assert_eq!(ErrorCode::from(-32000), ErrorCode::AuthenticationRequired);
        assert_eq!(ErrorCode::from(-32002), ErrorCode::ResourceNotFound);
    }
}
