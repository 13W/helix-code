//! MCP method dispatcher (PROTO §3): `initialize`, `ping`, `tools/list`,
//! `tools/call`, plus the notifications the CLI sends.

use serde_json::{json, Value};

use crate::jsonrpc::{Notification, Request, Response, RpcError};
use crate::tools::{self, SharedHandler, ToolResult};

/// Versions the VS Code extension echoes back (PROTO §3.2).
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SERVER_NAME: &str = "Claude Code Helix MCP";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct Dispatcher {
    handler: SharedHandler,
}

impl Dispatcher {
    pub fn new(handler: SharedHandler) -> Self {
        Dispatcher { handler }
    }

    pub fn handler(&self) -> &SharedHandler {
        &self.handler
    }

    pub async fn handle_request(&self, req: Request) -> Response {
        let Request { id, method, params } = req;
        let params = params.unwrap_or(Value::Null);
        let result = match method.as_str() {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::tool_list() })),
            "tools/call" => self.tools_call(&params).await,
            _ => Err(RpcError::method_not_found(&method)),
        };
        match result {
            Ok(value) => Response::success(id, value),
            Err(err) => Response::failure(id, err),
        }
    }

    pub fn handle_notification(&self, note: Notification) {
        match note.method.as_str() {
            // Sent by the SDK after `initialize`; nothing to do.
            "notifications/initialized" => {}
            // PROTO §3.4: carries the CLI pid, the extension ignores it too.
            "ide_connected" => {
                log::debug!("claude-ide: ide_connected {:?}", note.params);
            }
            "notifications/cancelled" => {
                log::debug!("claude-ide: request cancelled {:?}", note.params);
            }
            other => log::debug!("claude-ide: ignoring notification {other}"),
        }
    }

    async fn tools_call(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("Input validation error: name: Required"))?;
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        tools::validate_arguments(name, &arguments)?;
        let arguments = if arguments.is_null() {
            json!({})
        } else {
            arguments
        };
        // PROTO §3.7: a throwing handler becomes `isError: true`, not a JSON-RPC error.
        let result: ToolResult = match self.handler.call(name, arguments).await {
            Ok(result) => result,
            Err(err) => {
                log::warn!("claude-ide: tool {name} failed: {err:#}");
                ToolResult::error(err.to_string())
            }
        };
        serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = match requested {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        _ => DEFAULT_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::Id;
    use crate::tools::NotImplementedHandler;
    use std::sync::Arc;

    fn dispatcher() -> Dispatcher {
        Dispatcher::new(Arc::new(NotImplementedHandler))
    }

    fn req(method: &str, params: Value) -> Request {
        Request {
            id: Id::Num(7),
            method: method.into(),
            params: Some(params),
        }
    }

    #[tokio::test]
    async fn initialize_echoes_supported_version() {
        let r = dispatcher()
            .handle_request(req("initialize", json!({"protocolVersion": "2025-06-18"})))
            .await;
        let result = r.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(
            result["capabilities"],
            json!({"tools": {"listChanged": true}})
        );
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn initialize_falls_back_for_unknown_version() {
        let r = dispatcher()
            .handle_request(req("initialize", json!({"protocolVersion": "1999-01-01"})))
            .await;
        assert_eq!(
            r.result.unwrap()["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn unknown_method_is_32601() {
        let r = dispatcher()
            .handle_request(req("prompts/list", json!({})))
            .await;
        assert_eq!(r.error.unwrap().code, crate::jsonrpc::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let r = dispatcher()
            .handle_request(req(
                "tools/call",
                json!({"name": "openFile", "arguments": {}}),
            ))
            .await;
        let err = r.error.unwrap();
        assert_eq!(err.code, crate::jsonrpc::INVALID_PARAMS);
        assert_eq!(err.message, "Tool openFile not found");
    }

    #[tokio::test]
    async fn stub_handler_reports_tool_error() {
        let r = dispatcher()
            .handle_request(req(
                "tools/call",
                json!({"name": "closeAllDiffTabs", "arguments": {}}),
            ))
            .await;
        let result = r.result.unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["type"], "text");
    }
}
