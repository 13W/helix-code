//! JSON-RPC dispatch actor for ACP agents.
//!
//! `AgentRpcCall` is the internal message type sent over the mpsc channel
//! from `ClientHandle` to `rpc_actor`.  `rpc_actor` runs inside the
//! agent's `LocalSet` and translates each call into the appropriate SDK method.

use helix_acp_types::*;
use agent_client_protocol as sdk;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};

use crate::client::{AcpEvent, NewSessionResult, LoadSessionResult};
use crate::handler::{to_sdk_content_block, convert_stop_reason, convert_init_response};

// ---------------------------------------------------------------------------

pub(crate) enum AgentRpcCall {
    Initialize {
        reply: oneshot::Sender<Result<InitializeResult>>,
    },
    Authenticate {
        params: AuthenticateParams,
        reply: oneshot::Sender<Result<()>>,
    },
    NewSession {
        cwd: String,
        /// Address of the Helix MCP server to pass to the agent, if running.
        mcp_addr: Option<std::net::SocketAddr>,
        reply: oneshot::Sender<Result<NewSessionResult>>,
    },
    LoadSession {
        session_id: SessionId,
        mcp_addr: Option<std::net::SocketAddr>,
        reply: oneshot::Sender<Result<LoadSessionResult>>,
    },
    Prompt {
        session_id: SessionId,
        prompt: Vec<ContentBlock>,
        reply: oneshot::Sender<Result<StopReason>>,
    },
    Cancel {
        session_id: SessionId,
    },
    SetMode {
        session_id: SessionId,
        mode: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SetConfigOption {
        session_id: SessionId,
        option_id: String,
        value: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ListSessions {
        cwd: Option<String>,
        reply: oneshot::Sender<Result<Vec<ListedSession>>>,
    },
    AccountInfo {
        reply: oneshot::Sender<Result<AccountInfo>>,
    },
}

// ---------------------------------------------------------------------------

pub(crate) fn try_parse_usage_update(line: &[u8]) -> Option<(u64, u64, f64, String)> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    if v.get("method")?.as_str()? != "session/update" {
        return None;
    }
    let update = v.get("params")?.get("update")?;
    if update.get("sessionUpdate")?.as_str()? != "usage_update" {
        return None;
    }
    let used = update.get("used")?.as_u64()?;
    let size = update.get("size")?.as_u64()?;
    let cost = update.get("cost")?;
    let amount = cost.get("amount")?.as_f64()?;
    let currency = cost.get("currency")?.as_str()?.to_string();
    Some((used, size, amount, currency))
}

pub(crate) fn try_parse_turn_tokens(line: &[u8]) -> Option<(u64, u64, u64, u64)> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    v.get("id")?; // must be a response (has id)
    let usage = v.get("result")?.get("usage")?;
    let input = usage.get("inputTokens")?.as_u64()?;
    let output = usage.get("outputTokens")?.as_u64()?;
    let cache_read = usage.get("cachedReadTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_write = usage.get("cachedWriteTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    Some((input, output, cache_read, cache_write))
}

/// Rewrite outgoing JSON-RPC method names that claude-code-acp exposes without
/// the `_` prefix required by the ACP extension-method spec.
/// Returns the rewritten line (owned) only when a rewrite was performed.
pub(crate) fn rewrite_outgoing_method<'a>(
    line: &'a str,
    from: &str,
    to: &str,
) -> std::borrow::Cow<'a, str> {
    if !line.contains(from) {
        return std::borrow::Cow::Borrowed(line);
    }
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(line) {
        if v.get("method").and_then(|m| m.as_str()) == Some(from) {
            v["method"] = serde_json::Value::String(to.to_string());
            let mut s = v.to_string();
            s.push('\n');
            return std::borrow::Cow::Owned(s);
        }
    }
    std::borrow::Cow::Borrowed(line)
}

// ---------------------------------------------------------------------------

pub(crate) async fn rpc_actor(
    conn: Rc<sdk::ClientSideConnection>,
    mut rpc_rx: UnboundedReceiver<AgentRpcCall>,
    event_tx: UnboundedSender<(AgentId, AcpEvent)>,
    agent_id: AgentId,
) {
    use sdk::Agent as _;

    while let Some(call) = rpc_rx.recv().await {
        let conn = Rc::clone(&conn);
        let event_tx = event_tx.clone();
        tokio::task::spawn_local(async move {
            match call {
                AgentRpcCall::Initialize { reply } => {
                    let req = sdk::InitializeRequest::new(sdk::ProtocolVersion::LATEST)
                        .client_capabilities(
                            sdk::ClientCapabilities::new()
                                .fs(sdk::FileSystemCapabilities::new()
                                    .read_text_file(true)
                                    .write_text_file(true))
                                .terminal(false),
                        )
                        .client_info(
                            sdk::Implementation::new("helix", env!("CARGO_PKG_VERSION"))
                                .title("Helix Editor".to_owned()),
                        );
                    let result = conn.initialize(req).await
                        .map(convert_init_response)
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::Authenticate { params, reply } => {
                    let method_id = params
                        .extra
                        .get("methodId")
                        .or_else(|| params.extra.get("method"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("default")
                        .to_owned();
                    let req = sdk::AuthenticateRequest::new(method_id);
                    let result = conn.authenticate(req).await
                        .map(|_| ())
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::NewSession { cwd, mcp_addr, reply } => {
                    let mut req = sdk::NewSessionRequest::new(std::path::PathBuf::from(cwd));
                    if let Some(addr) = mcp_addr {
                        req = req.mcp_servers(vec![
                            sdk::McpServer::Http(sdk::McpServerHttp::new(
                                "helix",
                                format!("http://{addr}/mcp"),
                            )),
                        ]);
                    }
                    let result = conn.new_session(req).await
                        .map(|resp| NewSessionResult {
                            session_id: resp.session_id.to_string(),
                            config_options: resp.config_options.unwrap_or_default(),
                        })
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::LoadSession { session_id, mcp_addr, reply } => {
                    let cwd = std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("."));
                    let mut req = sdk::LoadSessionRequest::new(session_id, cwd);
                    if let Some(addr) = mcp_addr {
                        req = req.mcp_servers(vec![
                            sdk::McpServer::Http(sdk::McpServerHttp::new(
                                "helix",
                                format!("http://{addr}/mcp"),
                            )),
                        ]);
                    }
                    let result = conn.load_session(req).await
                        .map(|resp| LoadSessionResult {
                            config_options: resp.config_options.unwrap_or_default(),
                        })
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::Prompt { session_id, prompt, reply } => {
                    let sdk_prompt = prompt.into_iter().map(to_sdk_content_block).collect();
                    let req = sdk::PromptRequest::new(session_id, sdk_prompt);
                    let result = conn.prompt(req).await;
                    let result = result
                        .map(|resp| convert_stop_reason(resp.stop_reason))
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::Cancel { session_id } => {
                    let notif = sdk::CancelNotification::new(session_id);
                    let _ = conn.cancel(notif).await;
                }

                AgentRpcCall::SetMode { session_id, mode, reply } => {
                    let req = sdk::SetSessionModeRequest::new(session_id, mode);
                    let result = conn.set_session_mode(req).await
                        .map(|_| ())
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::SetConfigOption { session_id, option_id, value, reply } => {
                    let req = sdk::SetSessionConfigOptionRequest::new(session_id, option_id, value);
                    let result = conn.set_session_config_option(req).await;
                    if let Ok(ref resp) = result {
                        let _ = event_tx.send((agent_id, AcpEvent::ConfigOptionsUpdate(
                            resp.config_options.clone(),
                        )));
                    }
                    let result = result
                        .map(|_| ())
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::ListSessions { cwd, reply } => {
                    let params_json = if let Some(ref dir) = cwd {
                        serde_json::json!({ "cwd": dir })
                    } else {
                        serde_json::json!({})
                    };
                    let raw = serde_json::value::RawValue::from_string(params_json.to_string())
                        .unwrap_or_else(|_| {
                            serde_json::value::RawValue::from_string("{}".to_string()).unwrap()
                        });
                    let req = sdk::ExtRequest::new("session/list", Arc::from(raw));
                    let result = conn.ext_method(req).await
                        .map(|resp| {
                            let v: serde_json::Value = serde_json::from_str(resp.0.get())
                                .unwrap_or_default();
                            v["sessions"].as_array().map(|arr| {
                                arr.iter().filter_map(|s| Some(ListedSession {
                                    session_id: s["sessionId"].as_str()?.to_owned(),
                                    title: s["title"].as_str().unwrap_or("").to_owned(),
                                    cwd: s["cwd"].as_str().unwrap_or("").to_owned(),
                                    updated_at: s["updatedAt"].as_str().unwrap_or("").to_owned(),
                                })).collect()
                            }).unwrap_or_default()
                        })
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::AccountInfo { reply } => {
                    let raw = serde_json::value::RawValue::from_string("{}".to_string()).unwrap();
                    let req = sdk::ExtRequest::new("account/info", Arc::from(raw));
                    let result = conn.ext_method(req).await
                        .map(|resp| {
                            let v: serde_json::Value = serde_json::from_str(resp.0.get())
                                .unwrap_or_default();
                            AccountInfo {
                                email: v["emailAddress"].as_str()
                                    .or_else(|| v["email"].as_str())
                                    .map(str::to_owned),
                                name: v["name"].as_str()
                                    .or_else(|| v["displayName"].as_str())
                                    .map(str::to_owned),
                                account_uuid: v["accountUuid"].as_str()
                                    .or_else(|| v["id"].as_str())
                                    .map(str::to_owned),
                            }
                        })
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }
            }
        });
    }
}
