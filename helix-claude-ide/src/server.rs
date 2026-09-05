//! MCP method dispatcher (PROTO §3): `initialize`, `ping`, `tools/list`,
//! `tools/call`, plus the notifications the CLI sends.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::clients::{ClientId, Clients};
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
    clients: Arc<Clients>,
}

impl Dispatcher {
    pub fn new(handler: SharedHandler, clients: Arc<Clients>) -> Self {
        Dispatcher { handler, clients }
    }

    pub fn handler(&self) -> &SharedHandler {
        &self.handler
    }

    pub fn clients(&self) -> &Arc<Clients> {
        &self.clients
    }

    pub async fn handle_request(&self, client: ClientId, req: Request) -> Response {
        let Request { id, method, params } = req;
        let params = params.unwrap_or(Value::Null);
        let result = match method.as_str() {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::tool_list() })),
            "tools/call" => self.tools_call(client, &params).await,
            _ => Err(RpcError::method_not_found(&method)),
        };
        match result {
            Ok(value) => Response::success(id, value),
            Err(err) => Response::failure(id, err),
        }
    }

    pub fn handle_notification(&self, client: ClientId, note: Notification) {
        match note.method.as_str() {
            // Sent by the SDK after `initialize`; nothing to do.
            "notifications/initialized" => {}
            // PROTO §3.4: carries the CLI pid — remembered per connection (T8)
            // for `:claude-ide-status`, buffer names and `:claude-mention <pid>`.
            "ide_connected" => match ide_connected_pid(note.params.as_ref()) {
                Some(pid) => {
                    log::info!("claude-ide: client {client} is claude pid {pid}");
                    self.clients.set_pid(client, pid);
                }
                None => log::debug!("claude-ide: ide_connected without pid: {:?}", note.params),
            },
            "notifications/cancelled" => {
                log::debug!("claude-ide: request cancelled {:?}", note.params);
            }
            other => log::debug!("claude-ide: ignoring notification {other}"),
        }
    }

    async fn tools_call(&self, client: ClientId, params: &Value) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("Input validation error: name: Required"))?;
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        // PROTO §4.5 (T9.1): accepted but deliberately absent from `tools/list`.
        if name == tools::SET_PERMISSION_MODE {
            return self.set_permission_mode(client, &arguments);
        }
        tools::validate_arguments(name, &arguments)?;
        let arguments = if arguments.is_null() {
            json!({})
        } else {
            arguments
        };
        // PROTO §3.7: a throwing handler becomes `isError: true`, not a JSON-RPC error.
        let result: ToolResult = match self.handler.call(client, name, arguments).await {
            Ok(result) => result,
            Err(err) => {
                log::warn!("claude-ide: tool {name} failed: {err:#}");
                ToolResult::error(err.to_string())
            }
        };
        serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
    }

    /// `set_permission_mode {mode}`: remember the CLI's permission mode for
    /// this connection and answer `OK`.
    fn set_permission_mode(&self, client: ClientId, arguments: &Value) -> Result<Value, RpcError> {
        let mode = match arguments.get("mode") {
            Some(Value::String(mode)) => mode.as_str(),
            Some(_) => {
                return Err(RpcError::invalid_params(
                    "Input validation error: mode: Expected string",
                ))
            }
            None => return Err(RpcError::invalid_params("Input validation error: mode: Required")),
        };
        log::info!("claude-ide: client {client} permission mode -> {mode}");
        self.clients.set_permission_mode(client, mode);
        serde_json::to_value(ToolResult::text("OK")).map_err(|e| RpcError::internal(e.to_string()))
    }
}

/// `pid` of an `ide_connected` notification, if well-formed.
pub fn ide_connected_pid(params: Option<&Value>) -> Option<u32> {
    params?
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
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

    const CLIENT: ClientId = ClientId(1);

    fn dispatcher() -> Dispatcher {
        Dispatcher::new(Arc::new(NotImplementedHandler), Arc::new(Clients::new(4)))
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
            .handle_request(
                CLIENT,
                req("initialize", json!({"protocolVersion": "2025-06-18"})),
            )
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
            .handle_request(
                CLIENT,
                req("initialize", json!({"protocolVersion": "1999-01-01"})),
            )
            .await;
        assert_eq!(
            r.result.unwrap()["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn unknown_method_is_32601() {
        let r = dispatcher()
            .handle_request(CLIENT, req("prompts/list", json!({})))
            .await;
        assert_eq!(r.error.unwrap().code, crate::jsonrpc::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let r = dispatcher()
            .handle_request(
                CLIENT,
                req(
                    "tools/call",
                    json!({"name": "openFile", "arguments": {}}),
                ),
            )
            .await;
        let err = r.error.unwrap();
        assert_eq!(err.code, crate::jsonrpc::INVALID_PARAMS);
        assert_eq!(err.message, "Tool openFile not found");
    }

    #[tokio::test]
    async fn stub_handler_reports_tool_error() {
        let r = dispatcher()
            .handle_request(
                CLIENT,
                req(
                    "tools/call",
                    json!({"name": "closeAllDiffTabs", "arguments": {}}),
                ),
            )
            .await;
        let result = r.result.unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["type"], "text");
    }

    #[tokio::test]
    async fn set_permission_mode_is_accepted_but_unlisted() {
        let clients = Arc::new(Clients::new(4));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (close, _) = tokio::sync::watch::channel(false);
        let id = clients.try_insert(tx, close).unwrap();
        let d = Dispatcher::new(Arc::new(NotImplementedHandler), Arc::clone(&clients));

        let r = d
            .handle_request(
                id,
                req(
                    "tools/call",
                    json!({"name": "set_permission_mode", "arguments": {"mode": "plan"}}),
                ),
            )
            .await;
        assert_eq!(
            r.result.unwrap(),
            json!({"content": [{"type": "text", "text": "OK"}]})
        );
        assert_eq!(
            clients.snapshot(id).unwrap().permission_mode.as_deref(),
            Some("plan")
        );

        let missing = d
            .handle_request(
                id,
                req("tools/call", json!({"name": "set_permission_mode", "arguments": {}})),
            )
            .await;
        assert_eq!(missing.error.unwrap().code, crate::jsonrpc::INVALID_PARAMS);

        let list = d.handle_request(id, req("tools/list", json!({}))).await;
        assert_eq!(list.result.unwrap()["tools"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn ide_connected_records_pid() {
        let clients = Arc::new(Clients::new(4));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (close, _) = tokio::sync::watch::channel(false);
        let id = clients.try_insert(tx, close).unwrap();
        let d = Dispatcher::new(Arc::new(NotImplementedHandler), Arc::clone(&clients));
        d.handle_notification(
            id,
            Notification {
                method: "ide_connected".into(),
                params: Some(json!({"pid": 4242})),
            },
        );
        assert_eq!(clients.snapshot(id).unwrap().pid, Some(4242));
        assert_eq!(ide_connected_pid(Some(&json!({"pid": -1}))), None);
        assert_eq!(ide_connected_pid(None), None);
    }
}
