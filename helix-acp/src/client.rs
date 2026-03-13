//! ACP agent client — backed by the official `agent-client-protocol` SDK.
//!
//! Each agent runs in a dedicated `std::thread` with a `tokio::task::LocalSet`,
//! allowing the SDK's `!Send` connection types to work correctly.  The helix
//! main loop communicates with the LocalSet via ordinary mpsc channels.

use crate::{types::*, AgentId, Error, Result};
use agent_client_protocol as sdk;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use tokio::{
    process::{Child, Command},
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
        oneshot,
    },
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ---------------------------------------------------------------------------
// Public: AcpEvent
// ---------------------------------------------------------------------------

/// A reply channel for agent-initiated requests.  Wrapped in `Arc<Mutex<Option>>`
/// so it can be moved into `Fn` UI callbacks that may be invoked multiple times.
pub type ReplyChannel<T> = Arc<Mutex<Option<oneshot::Sender<T>>>>;

/// An event emitted by an ACP agent, forwarded to the application's main loop.
#[derive(Debug)]
pub enum AcpEvent {
    /// Agent sent a `session/update` notification.
    SessionNotification(sdk::SessionNotification),
    /// Agent requests user permission for a tool call.
    RequestPermission {
        params: sdk::RequestPermissionRequest,
        reply: ReplyChannel<sdk::RequestPermissionResponse>,
    },
    /// Agent wants to read a text file from the client's filesystem.
    ReadTextFile {
        params: sdk::ReadTextFileRequest,
        reply: ReplyChannel<sdk::ReadTextFileResponse>,
    },
    /// Agent wants to write a text file to the client's filesystem.
    WriteTextFile {
        params: sdk::WriteTextFileRequest,
        reply: ReplyChannel<sdk::WriteTextFileResponse>,
    },
    /// Agent subprocess disconnected / exited.
    Disconnected,
    /// Token cost from `session/update` with `usage_update`.
    UsageUpdate {
        used: u64,
        size: u64,
        amount: f64,
        currency: String,
    },
    /// Per-turn token counts from `session/prompt` stop result.
    TurnTokens {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Updated config options returned by `session/set_config_option`.
    ConfigOptionsUpdate(Vec<sdk::SessionConfigOption>),
}

// ---------------------------------------------------------------------------
// Public: ListedSession — result of a `session/list` ACP call
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Public: ListedSession — result of a `session/list` ACP call
// ---------------------------------------------------------------------------

/// A session entry returned by the ACP `session/list` extension method.
#[derive(Debug, Clone)]
pub struct ListedSession {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at: String,
}
// ---------------------------------------------------------------------------

enum AgentRpcCall {
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
// HelixClientHandler — implements sdk::Client for agent → client calls
// ---------------------------------------------------------------------------

struct HelixClientHandler {
    agent_id: AgentId,
    event_tx: UnboundedSender<(AgentId, AcpEvent)>,
}

#[async_trait::async_trait(?Send)]
impl sdk::Client for HelixClientHandler {
    async fn session_notification(&self, args: sdk::SessionNotification) -> sdk::Result<()> {
        let _ = self
            .event_tx
            .send((self.agent_id, AcpEvent::SessionNotification(args)));
        Ok(())
    }

    async fn request_permission(
        &self,
        args: sdk::RequestPermissionRequest,
    ) -> sdk::Result<sdk::RequestPermissionResponse> {
        let (tx, rx) = oneshot::channel();
        let reply = Arc::new(Mutex::new(Some(tx)));
        let _ = self.event_tx.send((
            self.agent_id,
            AcpEvent::RequestPermission { params: args, reply },
        ));
        rx.await.map_err(|_| sdk::Error::internal_error())
    }

    async fn read_text_file(
        &self,
        args: sdk::ReadTextFileRequest,
    ) -> sdk::Result<sdk::ReadTextFileResponse> {
        let (tx, rx) = oneshot::channel();
        let reply = Arc::new(Mutex::new(Some(tx)));
        let _ = self.event_tx.send((
            self.agent_id,
            AcpEvent::ReadTextFile { params: args, reply },
        ));
        rx.await.map_err(|_| sdk::Error::internal_error())
    }

    async fn write_text_file(
        &self,
        args: sdk::WriteTextFileRequest,
    ) -> sdk::Result<sdk::WriteTextFileResponse> {
        let (tx, rx) = oneshot::channel();
        let reply = Arc::new(Mutex::new(Some(tx)));
        let _ = self.event_tx.send((
            self.agent_id,
            AcpEvent::WriteTextFile { params: args, reply },
        ));
        rx.await.map_err(|_| sdk::Error::internal_error())
    }
}

// ---------------------------------------------------------------------------
// rpc_actor — runs in LocalSet; bridges AgentRpcCall → SDK calls
// ---------------------------------------------------------------------------

fn try_parse_usage_update(line: &[u8]) -> Option<(u64, u64, f64, String)> {
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

fn try_parse_turn_tokens(line: &[u8]) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    v.get("id")?; // must be a response (has id)
    let usage = v.get("result")?.get("usage")?;
    let input = usage.get("inputTokens")?.as_u64()?;
    let output = usage.get("outputTokens")?.as_u64()?;
    Some((input, output))
}

// Rewrite outgoing JSON-RPC method names that claude-code-acp exposes without
// the `_` prefix required by the ACP extension-method spec.
// Returns the rewritten line (owned) only when a rewrite was performed.
fn rewrite_outgoing_method<'a>(line: &'a str, from: &str, to: &str) -> std::borrow::Cow<'a, str> {
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
async fn rpc_actor(
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
                        .map(|resp| LoadSessionResult { config_options: resp.config_options.unwrap_or_default() })
                        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")));
                    let _ = reply.send(result);
                }

                AgentRpcCall::Prompt { session_id, prompt, reply } => {
                    let sdk_prompt = prompt.into_iter().map(to_sdk_content_block).collect();
                    let req = sdk::PromptRequest::new(session_id, sdk_prompt);
                    let result = conn.prompt(req).await;
                    if let Ok(ref resp) = result {
                        if let Some(ref usage) = resp.usage {
                            let _ = event_tx.send((agent_id, AcpEvent::TurnTokens {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                            }));
                        }
                    }
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
                    use sdk::Agent as _;
                    let params_json = if let Some(ref dir) = cwd {
                        serde_json::json!({ "cwd": dir })
                    } else {
                        serde_json::json!({})
                    };
                    let raw = serde_json::value::RawValue::from_string(params_json.to_string())
                        .unwrap_or_else(|_| serde_json::value::RawValue::from_string("{}".to_string()).unwrap());
                    let req = sdk::ExtRequest::new("session/list", std::sync::Arc::from(raw));
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
                    use sdk::Agent as _;
                    let raw = serde_json::value::RawValue::from_string("{}".to_string()).unwrap();
                    let req = sdk::ExtRequest::new("account/info", std::sync::Arc::from(raw));
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

// ---------------------------------------------------------------------------
// Type conversion helpers
// ---------------------------------------------------------------------------

fn to_sdk_content_block(cb: ContentBlock) -> sdk::ContentBlock {
    match cb {
        ContentBlock::Text { text } => sdk::ContentBlock::Text(sdk::TextContent::new(text)),
        // Fallback for non-text blocks — these are not used in current prompts
        _ => sdk::ContentBlock::Text(sdk::TextContent::new("[unsupported content block]")),
    }
}

fn convert_stop_reason(r: sdk::StopReason) -> StopReason {
    match r {
        sdk::StopReason::EndTurn => StopReason::EndTurn,
        sdk::StopReason::MaxTokens => StopReason::MaxTokens,
        sdk::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        sdk::StopReason::Refusal => StopReason::Refusal,
        sdk::StopReason::Cancelled => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

fn convert_init_response(resp: sdk::InitializeResponse) -> InitializeResult {
    let caps = resp.agent_capabilities;
    InitializeResult {
        protocol_version: caps.load_session as u16, // placeholder, not used
        capabilities: AgentCapabilities {
            load_session: Some(caps.load_session),
            prompt_capabilities: Some(PromptCapabilities {
                audio: caps.prompt_capabilities.audio,
                image: caps.prompt_capabilities.image,
                embedded_context: caps.prompt_capabilities.embedded_context,
            }),
            mcp_capabilities: None,
            session_capabilities: None,
        },
        agent_info: resp.agent_info.map(|i| AgentInfo {
            name: i.name,
            title: i.title,
            version: Some(i.version),
        }),
        auth_methods: resp
            .auth_methods
            .into_iter()
            .map(|m| AuthMethod {
                id: m.id().to_string(),
                name: m.name().to_owned(),
                description: m.description().map(|s| s.to_owned()),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for launching an ACP agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Binary name or absolute path.
    pub command: String,
    /// Arguments passed to the agent process.
    pub args: Vec<String>,
    /// Extra environment variables for the agent process.
    /// Extra environment variables for the agent process.
    pub env: std::collections::HashMap<String, String>,
    /// If set, write all ACP JSON-RPC lines to this JSONL file.
    pub log_path: Option<std::path::PathBuf>,
}

impl AgentConfig {
    pub fn new(command: impl Into<String>) -> Self {
        AgentConfig {
            command: command.into(),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            log_path: None,
        }
    }
}


// ---------------------------------------------------------------------------
// DisplayLine
// ---------------------------------------------------------------------------

/// A single entry in the agent panel display buffer.
#[derive(Debug, Clone)]
pub enum DisplayLine {
    /// Normal assistant text (may span multiple physical lines).
    Text(String),
    /// Internal thought chain — rendered dimmed.
    Thought(String),
    /// Tool call started: shows tool name while in progress.
    ToolCall { id: String, name: String, input: String, output: Vec<String> },
    /// Tool call finished — replaces the matching `ToolCall` entry in-place.
    ToolDone { id: String, name: String, input: String, status: String, output: Vec<String> },
    /// Plan step from a `PlanUpdate`.
    PlanStep { done: bool, description: String },
    /// Visual divider between conversation turns.
    Separator,
    /// The text the user sent — echoed in the panel.
    UserMessage(String),
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------
/// Accumulated token and cost statistics for the current session.
#[derive(Debug, Default)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_amount: f64,
    pub currency: String,
    /// Context window tokens used (from UsageUpdate).
    pub context_used: u64,
    /// Context window total size (from UsageUpdate).
    pub context_size: u64,
    /// Cumulative sum of `used` from all UsageUpdate events.
    pub total_used: u64,
}

/// An ACP agent client connected via stdio.
#[derive(Debug)]
pub struct Client {
    pub id: AgentId,
    pub name: String,
    _process: Child,
    rpc_tx: UnboundedSender<AgentRpcCall>,
    /// Negotiated agent capabilities, set after `initialize()` completes.
    pub capabilities: Option<AgentCapabilities>,
    /// The active session ID, set after `session_new()` or `session_load()`.
    pub session_id: Option<SessionId>,
    /// Auth methods declared by agent during initialize. Empty if none required.
    pub auth_methods: Vec<AuthMethod>,
    /// The original session_id passed via --resume, if this agent resumed a prior session.
    /// Used by the session picker to match running agents to JSONL file entries.
    pub resume_session_id: Option<String>,
    /// Structured display buffer, accumulated from `session/update` notifications.
    pub display: Vec<DisplayLine>,
    /// True while a `session/prompt` job is in flight.
    pub is_prompting: bool,
    /// Tracks file paths for in-progress "edit" tool calls.
    pub pending_edits: std::collections::HashMap<String, Vec<String>>,
    /// Current session mode received via `CurrentModeUpdate`.
    pub current_mode: Option<String>,
    /// Set to true by the permission dialog when the user selects an `AllowAlways` option.
    pub auto_continue: Arc<AtomicBool>,
    /// True after the user has selected "auto-accept edits" for this session.
    pub auto_accept_edits: bool,
    /// Accumulated token and cost statistics for the current session.
    pub session_usage: SessionUsage,
    /// Commands received via `AvailableCommandsUpdate`.
    pub available_commands: Vec<sdk::AvailableCommand>,
    /// Command text to drain into the textarea on the next panel event.
    pub pending_command: Option<String>,
    /// Session config options (model, mode, …) from `session/new` or `ConfigOptionUpdate`.
    pub config_options: Vec<sdk::SessionConfigOption>,
    /// Pending (option_id, value) to apply via `session_set_config_option` from the UI.
    pub pending_config_change: Option<(String, String)>,
    /// Pending reply channel + allow_always_id for a deferred "clean context" permission response.
    /// Set by the permission dialog; consumed by the main loop to send /clear then reply.
    pub pending_clean_context_reply: Option<(ReplyChannel<sdk::RequestPermissionResponse>, String)>,
    /// Authenticated user info, fetched after `authenticate()` + `session_new()` succeed.
    pub account_info: Option<AccountInfo>,
}

/// Build one JSONL log entry: `{"ts":<unix_ms>,"dir":"<dir>","line":<escaped>}`.
fn make_log_entry(dir: &str, line: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let trimmed = line.trim_end();
    let escaped = serde_json::to_string(trimmed).unwrap_or_default();
    format!(r#"{{"ts":{ts},"dir":"{dir}","line":{escaped}}}"#)
}

impl Client {
    pub fn start(
        config: &AgentConfig,
        id: AgentId,
    ) -> Result<(Self, UnboundedReceiver<(AgentId, AcpEvent)>)> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut process = cmd.spawn().map_err(|err| {
            anyhow::anyhow!("failed to spawn agent '{}': {err}", config.command)
        })?;

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("agent stdin unavailable"))?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("agent stdout unavailable"))?;
        let stderr = process
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("agent stderr unavailable"))?;

        let name = config.command.clone();
        let log_path = config.log_path.clone();
        let agent_id = id;


        let (event_tx, event_rx) = unbounded_channel::<(AgentId, AcpEvent)>();
        let (rpc_tx, rpc_rx) = unbounded_channel::<AgentRpcCall>();

        let event_tx_handler = event_tx.clone();
        let event_tx_tee = event_tx.clone();
        let event_tx_rpc = event_tx.clone();
        let event_tx_io = event_tx;

        std::thread::Builder::new()
            .name(format!("acp-{agent_id}"))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build ACP tokio runtime");

                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    let handler = HelixClientHandler {
                        agent_id,
                        event_tx: event_tx_handler,
                    };

                    // --- optional JSONL protocol logger ---
                    let log_tx_recv: Option<tokio::sync::mpsc::UnboundedSender<String>>;
                    let log_tx_send: Option<tokio::sync::mpsc::UnboundedSender<String>>;
                    if let Some(ref path) = log_path {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match tokio::fs::File::create(path).await {
                            Ok(file) => {
                                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                let mut w = tokio::io::BufWriter::new(file);
                                tokio::task::spawn_local(async move {
                                    use tokio::io::AsyncWriteExt;
                                    while let Some(entry) = rx.recv().await {
                                        let _ = w.write_all(entry.as_bytes()).await;
                                        let _ = w.write_all(b"\n").await;
                                        let _ = w.flush().await;
                                    }
                                });
                                log_tx_recv = Some(tx.clone());
                                log_tx_send = Some(tx);
                            }
                            Err(e) => {
                                log::warn!("ACP: cannot create log file {path:?}: {e}");
                                log_tx_recv = None;
                                log_tx_send = None;
                            }
                        }
                    } else {
                        log_tx_recv = None;
                        log_tx_send = None;
                    }

                    // Intercept stdout to parse usage_update and per-turn token counts.
                    let (duplex_sdk, duplex_agent) = tokio::io::duplex(64 * 1024);
                    tokio::task::spawn_local(async move {
                        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                        let mut reader = BufReader::new(stdout);
                        let mut writer = duplex_agent;
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) => break,
                                Ok(_) => {
                                    let bytes = line.as_bytes();
                                    if let Some((used, size, amount, currency)) =
                                        try_parse_usage_update(bytes)
                                    {
                                        let _ = event_tx_tee.send((
                                            agent_id,
                                            AcpEvent::UsageUpdate {
                                                used,
                                                size,
                                                amount,
                                                currency,
                                            },
                                        ));
                                    }
                                    if let Some((input, output)) = try_parse_turn_tokens(bytes) {
                                        let _ = event_tx_tee.send((
                                            agent_id,
                                            AcpEvent::TurnTokens {
                                                input_tokens: input,
                                                output_tokens: output,
                                            },
                                        ));
                                    }
                                    if let Some(ref tx) = log_tx_recv {
                                        let _ = tx.send(make_log_entry("recv", &line));
                                    }
                                    if writer.write_all(bytes).await.is_err() {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });

                    // Intercept stdin to rewrite extension method names that
                    // claude-code-acp exposes without the ACP `_` prefix.
                    let (duplex_stdin_sdk, duplex_stdin_agent) = tokio::io::duplex(64 * 1024);
                    tokio::task::spawn_local(async move {
                        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                        let mut reader = BufReader::new(duplex_stdin_agent);
                        let mut writer = stdin;
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) => break,
                                Ok(_) => {
                                     let out = rewrite_outgoing_method(&line, "_session/list", "session/list");
                                     let out = rewrite_outgoing_method(&out, "_account/info", "account/info");
                                    if let Some(ref tx) = log_tx_send {
                                        let _ = tx.send(make_log_entry("send", &out));
                                    }
                                    if writer.write_all(out.as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });

                    // SDK reads from duplex instead of raw stdout.
                    let (conn, io_task) = sdk::ClientSideConnection::new(
                        handler,
                        duplex_stdin_sdk.compat_write(),
                        duplex_sdk.compat(),
                        |fut| {
                            tokio::task::spawn_local(fut);
                        },
                    );

                    let conn = Rc::new(conn);

                    // Log stderr from the agent subprocess.
                    tokio::task::spawn_local(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            log::debug!("ACP {agent_id} stderr: {line}");
                        }
                    });

                    // Drive the SDK I/O loop; send Disconnected when it ends.
                    tokio::task::spawn_local(async move {
                        if let Err(e) = io_task.await {
                            log::error!("ACP {agent_id}: I/O error: {e}");
                        }
                        log::info!("ACP {agent_id}: disconnected");
                        let _ = event_tx_io.send((agent_id, AcpEvent::Disconnected));
                    });

                    // Process outgoing RPC calls until the channel closes.
                    rpc_actor(conn, rpc_rx, event_tx_rpc, agent_id).await;
                });
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn ACP thread: {e}"))?;

        let client = Client {
            id,
            name,
            _process: process,
            rpc_tx,
            capabilities: None,
            session_id: None,
            auth_methods: Vec::new(),
            resume_session_id: None,
            display: Vec::new(),
            is_prompting: false,
            pending_edits: std::collections::HashMap::new(),
            current_mode: None,
            auto_continue: Arc::new(AtomicBool::new(false)),
            auto_accept_edits: false,
            session_usage: SessionUsage::default(),
            available_commands: Vec::new(),
            pending_command: None,
            config_options: Vec::new(),
            pending_config_change: None,
            pending_clean_context_reply: None,
            account_info: None,
        };

        Ok((client, event_rx))
    }

    // ------------------------------------------------------------------
    // Lifecycle methods (synchronous-ish wrappers calling ClientHandle methods)
    // ------------------------------------------------------------------

    pub async fn initialize(&mut self) -> Result<()> {
        let handle = self.handle();
        let result = handle.initialize().await?;
        log::info!(
            "ACP agent '{}' initialized (protocol_version={})",
            self.name,
            result.protocol_version
        );
        self.capabilities = Some(result.capabilities);
        self.auth_methods = result.auth_methods;
        Ok(())
    }

    pub async fn authenticate(&self, params: AuthenticateParams) -> Result<()> {
        self.handle().authenticate(params).await
    }

    pub async fn session_new(
        &mut self,
        cwd: String,
        mcp_addr: Option<std::net::SocketAddr>,
    ) -> Result<SessionId> {
        let result = self.handle().session_new(cwd, mcp_addr).await?;
        let sid = result.session_id.clone();
        self.session_id = Some(sid.clone());
        self.config_options = result.config_options;
        log::info!("ACP agent '{}' session created: {}", self.name, sid);
        Ok(sid)
    }

    pub async fn session_load(
        &mut self,
        session_id: SessionId,
        mcp_addr: Option<std::net::SocketAddr>,
    ) -> Result<LoadSessionResult> {
        let result = self.handle().session_load(session_id.clone(), mcp_addr).await?;
        self.session_id = Some(session_id);
        self.config_options = result.config_options.clone();
        Ok(result)
    }

    pub async fn session_prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<StopReason> {
        self.handle().session_prompt(session_id, prompt).await
    }

    pub fn session_cancel(&self, session_id: SessionId) {
        let _ = self.rpc_tx.send(AgentRpcCall::Cancel { session_id });
    }

    pub async fn session_set_mode(
        &self,
        session_id: SessionId,
        mode: String,
    ) -> Result<()> {
        self.handle().session_set_mode(session_id, mode).await
    }

    pub async fn session_set_config_option(
        &self,
        session_id: SessionId,
        option_id: String,
        value: String,
    ) -> Result<()> {
        self.handle().session_set_config_option(session_id, option_id, value).await
    }

    pub async fn session_list(&self, cwd: Option<String>) -> Result<Vec<ListedSession>> {
        self.handle().session_list(cwd).await
    }

    pub async fn account_info(&self) -> Result<AccountInfo> {
        self.handle().account_info().await
    }

    pub async fn prompt_text(&self, text: impl Into<String>) -> Result<StopReason> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no active session — call session_new() first"))?;
        self.session_prompt(session_id, vec![ContentBlock::text(text)]).await
    }

    /// Create a lightweight cloneable handle for use in background tasks.
    pub fn handle(&self) -> ClientHandle {
        ClientHandle {
            id: self.id,
            name: self.name.clone(),
            rpc_tx: self.rpc_tx.clone(),
        }
    }

    /// Concatenate all [`DisplayLine::Text`] entries into a single string.
    pub fn response_text(&self) -> String {
        self.display
            .iter()
            .filter_map(|l| {
                if let DisplayLine::Text(s) = l {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ClientHandle — a Send + Clone handle for use in async Jobs
// ---------------------------------------------------------------------------

/// A lightweight cloneable handle for use in background tasks (Jobs).
#[derive(Clone)]
pub struct ClientHandle {
    pub id: AgentId,
    name: String,
    rpc_tx: UnboundedSender<AgentRpcCall>,
}

impl ClientHandle {
    /// Helper: send an RPC call and await the reply.
    async fn call<T>(
        &self,
        make_call: impl FnOnce(oneshot::Sender<Result<T>>) -> AgentRpcCall,
    ) -> Result<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.rpc_tx
            .send(make_call(reply_tx))
            .map_err(|_| Error::StreamClosed)?;
        reply_rx.await.map_err(|_| Error::StreamClosed)?
    }

    pub async fn initialize(&self) -> Result<InitializeResult> {
        let result = self.call(|reply| AgentRpcCall::Initialize { reply }).await?;
        log::info!(
            "ACP agent '{}' initialized (protocol_version={})",
            self.name,
            result.protocol_version
        );
        Ok(result)
    }

    pub async fn authenticate(&self, params: AuthenticateParams) -> Result<()> {
        self.call(|reply| AgentRpcCall::Authenticate { params, reply }).await
    }

    pub async fn session_new(
        &self,
        cwd: String,
        mcp_addr: Option<std::net::SocketAddr>,
    ) -> Result<NewSessionResult> {
        let result = self
            .call(|reply| AgentRpcCall::NewSession { cwd, mcp_addr, reply })
            .await?;
        log::info!("ACP agent '{}' session created: {}", self.name, result.session_id);
        Ok(result)
    }

    pub async fn session_load(
        &self,
        session_id: SessionId,
        mcp_addr: Option<std::net::SocketAddr>,
    ) -> Result<LoadSessionResult> {
        self.call(|reply| AgentRpcCall::LoadSession { session_id, mcp_addr, reply }).await
    }

    pub async fn session_prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<StopReason> {
        self.call(|reply| AgentRpcCall::Prompt { session_id, prompt, reply }).await
    }

    pub fn session_cancel(&self, session_id: SessionId) -> Result<()> {
        self.rpc_tx
            .send(AgentRpcCall::Cancel { session_id })
            .map_err(|_| Error::StreamClosed)
    }

    pub async fn session_set_mode(
        &self,
        session_id: SessionId,
        mode: String,
    ) -> Result<()> {
        self.call(|reply| AgentRpcCall::SetMode { session_id, mode, reply }).await
    }

    pub async fn session_set_config_option(
        &self,
        session_id: SessionId,
        option_id: String,
        value: String,
    ) -> Result<()> {
        self.call(|reply| AgentRpcCall::SetConfigOption { session_id, option_id, value, reply })
            .await
    }

    pub async fn session_list(&self, cwd: Option<String>) -> Result<Vec<ListedSession>> {
        self.call(|reply| AgentRpcCall::ListSessions { cwd, reply }).await
    }

    pub async fn account_info(&self) -> Result<AccountInfo> {
        self.call(|reply| AgentRpcCall::AccountInfo { reply }).await
    }
}
