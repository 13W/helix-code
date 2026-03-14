//! ACP agent client — backed by the official `agent-client-protocol` SDK.
//!
//! Each agent runs in a dedicated `std::thread` with a `tokio::task::LocalSet`,
//! allowing the SDK's `!Send` connection types to work correctly.  The helix
//! main loop communicates with the LocalSet via ordinary mpsc channels.

use helix_acp_types::*;
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

use crate::handler::HelixClientHandler;
use crate::rpc::{AgentRpcCall, rpc_actor, try_parse_usage_update, try_parse_turn_tokens,
                 rewrite_outgoing_method};

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
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    },
    /// Updated config options returned by `session/set_config_option`.
    ConfigOptionsUpdate(Vec<sdk::SessionConfigOption>),
}

// ---------------------------------------------------------------------------
// SDK-typed results (stay in helix-acp due to sdk dep)
// ---------------------------------------------------------------------------

/// Result of a successful `session/new` call.
#[derive(Debug, Clone)]
pub struct NewSessionResult {
    pub session_id: SessionId,
    /// Configuration options (model, mode, …) received in the `session/new` response.
    pub config_options: Vec<sdk::SessionConfigOption>,
}

/// Result of a successful `session/load` call.
#[derive(Debug, Clone)]
pub struct LoadSessionResult {
    /// Configuration options (model, mode, …) received in the `session/load` response.
    pub config_options: Vec<sdk::SessionConfigOption>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

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
                                    if let Some((input, output, cache_read, cache_write)) = try_parse_turn_tokens(bytes) {
                                        let _ = event_tx_tee.send((
                                            agent_id,
                                            AcpEvent::TurnTokens {
                                                input_tokens: input,
                                                output_tokens: output,
                                                cache_read_tokens: cache_read,
                                                cache_write_tokens: cache_write,
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
                                    let out = rewrite_outgoing_method(
                                        &line, "_session/list", "session/list");
                                    let out = rewrite_outgoing_method(
                                        &out, "_account/info", "account/info");
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
