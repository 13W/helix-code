//! Tool surface exposed to the CLI: static `tools/list` schemas, the
//! [`ToolHandler`] trait implemented by the editor side, and the result type.
//!
//! Only the four tools the CLI actually calls are published (PROTO §0, §4).
//! Schemas mirror what `zodToJsonSchema` produces in the VS Code extension:
//! draft-07, `additionalProperties: false`, descriptions verbatim.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::clients::ClientId;
use crate::jsonrpc::RpcError;

pub const OPEN_DIFF: &str = "openDiff";
pub const CLOSE_TAB: &str = "close_tab";
pub const CLOSE_ALL_DIFF_TABS: &str = "closeAllDiffTabs";
pub const GET_DIAGNOSTICS: &str = "getDiagnostics";

/// Called by the CLI when its permission mode changes (PROTO §4.5). Accepted
/// by the dispatcher, stored per client, never published in `tools/list`.
pub const SET_PERMISSION_MODE: &str = "set_permission_mode";

pub const TOOL_NAMES: [&str; 4] = [OPEN_DIFF, CLOSE_TAB, CLOSE_ALL_DIFF_TABS, GET_DIAGNOSTICS];

const JSON_SCHEMA_DRAFT07: &str = "http://json-schema.org/draft-07/schema#";

/// The `tools` array returned by `tools/list`.
pub fn tool_list() -> Value {
    let path_desc = "Path to the file to show diff for. If not provided, uses active editor.";
    json!([
        {
            "name": OPEN_DIFF,
            "description": "Open a git diff for the file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "old_file_path": { "type": "string", "description": path_desc },
                    "new_file_path": { "type": "string", "description": path_desc },
                    "new_file_contents": {
                        "type": "string",
                        "description": "Contents of the new file. If not provided then the current file contents of new_file_path will be used."
                    },
                    "tab_name": { "type": "string", "description": path_desc }
                },
                "required": ["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
                "additionalProperties": false,
                "$schema": JSON_SCHEMA_DRAFT07
            }
        },
        {
            "name": CLOSE_TAB,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_name": { "type": "string" }
                },
                "required": ["tab_name"],
                "additionalProperties": false,
                "$schema": JSON_SCHEMA_DRAFT07
            }
        },
        {
            "name": CLOSE_ALL_DIFF_TABS,
            "description": "Close all diff tabs in the editor",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": GET_DIAGNOSTICS,
            "description": "Get language diagnostics from VS Code",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "Optional file URI to get diagnostics for. If not provided, gets diagnostics for all files."
                    }
                },
                "additionalProperties": false,
                "$schema": JSON_SCHEMA_DRAFT07
            }
        }
    ])
}

/// Validate `tools/call` arguments against the published schema.
///
/// Mirrors the SDK: unknown tool and shape errors are JSON-RPC
/// `InvalidParams`, everything else is left to the handler.
pub fn validate_arguments(name: &str, arguments: &Value) -> Result<(), RpcError> {
    if !TOOL_NAMES.contains(&name) {
        return Err(RpcError::invalid_params(format!("Tool {name} not found")));
    }
    let obj = match arguments {
        Value::Object(map) => map,
        Value::Null => {
            return match name {
                OPEN_DIFF | CLOSE_TAB => Err(validation_error("arguments must be an object")),
                _ => Ok(()),
            }
        }
        _ => return Err(validation_error("arguments must be an object")),
    };
    let required: &[&str] = match name {
        OPEN_DIFF => &[
            "old_file_path",
            "new_file_path",
            "new_file_contents",
            "tab_name",
        ],
        CLOSE_TAB => &["tab_name"],
        _ => &[],
    };
    let optional: &[&str] = match name {
        GET_DIAGNOSTICS => &["uri"],
        _ => &[],
    };
    for field in required {
        match obj.get(*field) {
            Some(Value::String(_)) => {}
            Some(_) => return Err(validation_error(format!("{field}: Expected string"))),
            None => return Err(validation_error(format!("{field}: Required"))),
        }
    }
    for field in optional {
        if let Some(v) = obj.get(*field) {
            if !v.is_string() && !v.is_null() {
                return Err(validation_error(format!("{field}: Expected string")));
            }
        }
    }
    if name != CLOSE_ALL_DIFF_TABS {
        if let Some(extra) = obj
            .keys()
            .find(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str()))
        {
            return Err(validation_error(format!("Unrecognized key: '{extra}'")));
        }
    }
    Ok(())
}

fn validation_error(detail: impl std::fmt::Display) -> RpcError {
    RpcError::invalid_params(format!("Input validation error: {detail}"))
}

/// One `content` item of a tool result. The protocol only uses text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text { text: String },
}

/// `tools/call` result body (`{content: [...], isError?: true}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![Content::Text { text: text.into() }],
            is_error: false,
        }
    }

    pub fn texts(texts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        ToolResult {
            content: texts
                .into_iter()
                .map(|t| Content::Text { text: t.into() })
                .collect(),
            is_error: false,
        }
    }

    /// What the SDK's `createToolError` produces when a handler throws.
    pub fn error(message: impl Into<String>) -> Self {
        ToolResult {
            content: vec![Content::Text {
                text: message.into(),
            }],
            is_error: true,
        }
    }
}

/// Editor-side implementation of the tools plus connection lifecycle hooks.
///
/// `call` may block for as long as it likes (`openDiff` waits for the user);
/// the transport runs each request on its own task, so a long call never
/// delays `close_tab` or `closeAllDiffTabs`. Several CLI clients may be
/// connected at once (T8): every call carries the [`ClientId`] it came from,
/// and per-client state (pending diffs) must be keyed by it.
#[async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    async fn call(
        &self,
        client: ClientId,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<ToolResult>;

    /// A client finished the WebSocket upgrade. `notifier` addresses every
    /// client of this server (`notify_all`) or one of them (`notify_one`).
    fn on_client_connected(&self, _client: ClientId, _notifier: crate::Notifier) {}

    /// `client` went away (socket closed, disconnected by the user, or the
    /// server stopped). Its pending `openDiff` calls should be resolved as
    /// rejected here; other clients are unaffected.
    fn on_client_disconnected(&self, _client: ClientId) {}
}

/// Placeholder used until the editor wires real tools in: every call is
/// reported back as a tool error, never as a transport failure.
#[derive(Debug, Default)]
pub struct NotImplementedHandler;

#[async_trait]
impl ToolHandler for NotImplementedHandler {
    async fn call(
        &self,
        _client: ClientId,
        name: &str,
        _arguments: Value,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error(format!("{name}: not implemented")))
    }
}

pub type SharedHandler = Arc<dyn ToolHandler>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_has_four_object_schemas() {
        let list = tool_list();
        let tools = list.as_array().unwrap();
        assert_eq!(tools.len(), 4);
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(TOOL_NAMES.contains(&tool["name"].as_str().unwrap()));
        }
        assert!(
            tools[1].get("description").is_none(),
            "close_tab has no description"
        );
    }

    #[test]
    fn validation() {
        assert!(validate_arguments("nope", &json!({})).is_err());
        assert!(validate_arguments(CLOSE_ALL_DIFF_TABS, &Value::Null).is_ok());
        assert!(validate_arguments(GET_DIAGNOSTICS, &json!({})).is_ok());
        assert!(validate_arguments(GET_DIAGNOSTICS, &json!({"uri": "file:///x"})).is_ok());
        assert!(validate_arguments(GET_DIAGNOSTICS, &json!({"uri": 5})).is_err());
        assert!(validate_arguments(CLOSE_TAB, &json!({})).is_err());
        assert!(validate_arguments(CLOSE_TAB, &json!({"tab_name": "t"})).is_ok());
        assert!(validate_arguments(CLOSE_TAB, &json!({"tab_name": "t", "x": 1})).is_err());
        let full =
            json!({"old_file_path":"a","new_file_path":"a","new_file_contents":"","tab_name":"t"});
        assert!(validate_arguments(OPEN_DIFF, &full).is_ok());
        let err = validate_arguments(OPEN_DIFF, &json!({"old_file_path":"a"})).unwrap_err();
        assert_eq!(err.code, crate::jsonrpc::INVALID_PARAMS);
        assert!(err.message.starts_with("Input validation error"));
    }

    #[test]
    fn result_serialization() {
        assert_eq!(
            serde_json::to_value(ToolResult::texts(["FILE_SAVED", "body"])).unwrap(),
            json!({"content":[{"type":"text","text":"FILE_SAVED"},{"type":"text","text":"body"}]})
        );
        assert_eq!(
            serde_json::to_value(ToolResult::error("boom")).unwrap(),
            json!({"content":[{"type":"text","text":"boom"}],"isError":true})
        );
    }
}
