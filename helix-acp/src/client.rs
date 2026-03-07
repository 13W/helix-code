//! ACP agent client.
//!
//! `Client` manages a single agent subprocess: spawning the process, performing
//! the initialization handshake, and providing typed methods for all ACP requests.

use crate::{
    jsonrpc,
    transport::{self, Payload},
    types::*,
    AgentId, Error, Result,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, Command},
    sync::mpsc::{channel, UnboundedReceiver, UnboundedSender},
};

/// Current ACP protocol version supported by this client.
pub const PROTOCOL_VERSION: ProtocolVersion = 1;

/// Configuration for launching an ACP agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Binary name or absolute path.
    pub command: String,
    /// Arguments passed to the agent process.
    pub args: Vec<String>,
    /// Extra environment variables for the agent process.
    pub env: std::collections::HashMap<String, String>,
}

impl AgentConfig {
    pub fn new(command: impl Into<String>) -> Self {
        AgentConfig {
            command: command.into(),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
        }
    }
}

/// A single entry in the agent panel display buffer.
///
/// Pushed by `handle_acp_message` for every `SessionUpdate` variant.
/// Rendered by `AgentPanel` with per-kind styles.
#[derive(Debug, Clone)]
pub enum DisplayLine {
    /// Normal assistant text (may span multiple physical lines).
    Text(String),
    /// Internal thought chain — rendered dimmed.
    Thought(String),
    /// Tool call started: shows tool name while in progress.
    ToolCall { id: String, name: String },
    /// Tool call finished — replaces the matching `ToolCall` entry in-place.
    ToolDone { id: String, status: String },
    /// Plan step from a `PlanUpdate`.
    PlanStep { done: bool, description: String },
}

/// An ACP agent client connected via stdio.
#[derive(Debug)]
pub struct Client {
    pub id: AgentId,
    pub name: String,
    _process: Child,
    server_tx: UnboundedSender<Payload>,
    request_counter: Arc<AtomicU64>,
    /// Negotiated agent capabilities, set after `initialize()` completes.
    pub capabilities: Option<AgentCapabilities>,
    /// The active session ID, set after `session_new()` or `session_load()`.
    pub session_id: Option<SessionId>,
    /// Structured display buffer, accumulated from `session/update` notifications.
    /// Cleared at the start of each new prompt.
    pub display: Vec<DisplayLine>,
    /// True while a `session/prompt` job is in flight. Used by AgentPanel to show spinner.
    pub is_prompting: bool,
}

impl Client {
    /// Spawn the agent subprocess and set up the transport.
    ///
    /// Returns the client and an incoming message channel.  The caller is
    /// responsible for polling the channel and dispatching agent-initiated
    /// requests (`fs/*`, `terminal/*`, `session/request_permission`) and
    /// notifications (`session/update`, `$/disconnected`).
    pub fn start(
        config: &AgentConfig,
        id: AgentId,
    ) -> Result<(Self, UnboundedReceiver<(AgentId, jsonrpc::Call)>)> {
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
        let (rx, tx) = transport::Transport::start(
            BufReader::new(stdout),
            BufWriter::new(stdin),
            BufReader::new(stderr),
            id,
            name.clone(),
        );

        let client = Client {
            id,
            name,
            _process: process,
            server_tx: tx,
            request_counter: Arc::new(AtomicU64::new(0)),
            capabilities: None,
            session_id: None,
            display: Vec::new(),
            is_prompting: false,
        };

        Ok((client, rx))
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a request and await the response, deserializing it into `R`.
    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R> {
        let id = self.next_request_id();

        let params_value = serde_json::to_value(&params)?;
        let params = match params_value {
            Value::Object(map) => jsonrpc::Params::Map(map),
            Value::Array(arr) => jsonrpc::Params::Array(arr),
            Value::Null => jsonrpc::Params::None,
            other => jsonrpc::Params::Array(vec![other]),
        };

        let method_call = jsonrpc::MethodCall {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_owned(),
            params,
            id: jsonrpc::Id::Num(id),
        };

        // Channel capacity 1 — exactly one response per request.
        let (tx, mut rx) = channel(1);
        self.server_tx
            .send(Payload::Request {
                chan: tx,
                value: method_call,
            })
            .map_err(|_| Error::StreamClosed)?;

        let value = rx.recv().await.ok_or(Error::StreamClosed)??;
        serde_json::from_value(value).map_err(Into::into)
    }

    /// Send a notification (no response expected).
    pub fn notify(&self, method: &str, params: impl Serialize) {
        let params_value = match serde_json::to_value(&params) {
            Ok(v) => v,
            Err(err) => {
                log::error!("{}: failed to serialize notification '{method}': {err}", self.name);
                return;
            }
        };
        let params = match params_value {
            Value::Object(map) => jsonrpc::Params::Map(map),
            Value::Array(arr) => jsonrpc::Params::Array(arr),
            Value::Null => jsonrpc::Params::None,
            other => jsonrpc::Params::Array(vec![other]),
        };

        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_owned(),
            params,
        };
        let _ = self.server_tx.send(Payload::Notification(notification));
    }

    /// Send a response to an agent-initiated request.
    pub fn reply(&self, id: jsonrpc::Id, result: Result<Value>) {
        let output = match result {
            Ok(value) => jsonrpc::Output::Success(jsonrpc::Success {
                jsonrpc: Some(jsonrpc::Version::V2),
                result: value,
                id,
            }),
            Err(err) => jsonrpc::Output::Failure(jsonrpc::Failure {
                jsonrpc: Some(jsonrpc::Version::V2),
                error: jsonrpc::Error::internal_error(err.to_string()),
                id,
            }),
        };
        let _ = self.server_tx.send(Payload::Response(output));
    }

    /// Send an error response to an agent-initiated request.
    pub fn reply_error(&self, id: jsonrpc::Id, error: jsonrpc::Error) {
        let output = jsonrpc::Output::Failure(jsonrpc::Failure {
            jsonrpc: Some(jsonrpc::Version::V2),
            error,
            id,
        });
        let _ = self.server_tx.send(Payload::Response(output));
    }

    /// Return a cloneable sender for use in `'static` callbacks.
    pub fn sender(&self) -> UnboundedSender<Payload> {
        self.server_tx.clone()
    }

    // ------------------------------------------------------------------
    // ACP lifecycle methods
    // ------------------------------------------------------------------

    /// Perform the `initialize` handshake, negotiating protocol version and
    /// exchanging capabilities.  Must be called first before any other method.
    pub async fn initialize(&mut self) -> Result<()> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities {
                filesystem: Some(FileSystemCapabilities {
                    read_text_file: true,
                    write_text_file: true,
                }),
                terminal: Some(TerminalCapabilities::default()),
            },
            client_info: Some(ClientInfo {
                name: "helix".to_owned(),
                title: Some("Helix Editor".to_owned()),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        };

        let result: InitializeResult = self.call("initialize", params).await?;
        log::info!(
            "ACP agent '{}' initialized (protocol_version={})",
            self.name,
            result.protocol_version
        );
        self.capabilities = Some(result.capabilities);
        Ok(())
    }

    /// Authenticate with the agent.  In the simplest case this sends an empty
    /// map; the agent may require additional fields via the `extra` map.
    pub async fn authenticate(&self, params: AuthenticateParams) -> Result<()> {
        let _: AuthenticateResult = self.call("authenticate", params).await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Session methods
    // ------------------------------------------------------------------

    /// Create a new conversation session.
    pub async fn session_new(&mut self) -> Result<SessionId> {
        let result: NewSessionResult = self
            .call("session/new", NewSessionParams::default())
            .await?;
        self.session_id = Some(result.session_id.clone());
        log::info!("ACP agent '{}' session created: {}", self.name, result.session_id);
        Ok(result.session_id)
    }

    /// Resume a previous conversation session.
    pub async fn session_load(&mut self, session_id: SessionId) -> Result<()> {
        let _: LoadSessionResult = self
            .call("session/load", LoadSessionParams { session_id: session_id.clone() })
            .await?;
        self.session_id = Some(session_id);
        Ok(())
    }

    /// Send a user prompt and wait for the agent's stop reason.
    ///
    /// While waiting, the agent will emit `session/update` notifications via
    /// the incoming channel (streamed message chunks, tool calls, plans, …).
    pub async fn session_prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<StopReason> {
        let result: PromptResult = self
            .call("session/prompt", PromptParams { session_id, prompt })
            .await?;
        Ok(result.stop_reason)
    }

    /// Cancel an ongoing prompt turn (one-way notification, no response).
    pub fn session_cancel(&self, session_id: SessionId) {
        self.notify("session/cancel", CancelParams { session_id });
    }

    /// Change the active mode of a session.
    pub async fn session_set_mode(
        &self,
        session_id: SessionId,
        mode: String,
    ) -> Result<()> {
        let _: SetModeResult = self
            .call("session/set_mode", SetModeParams { session_id, mode })
            .await?;
        Ok(())
    }

    /// Set a session configuration option.
    pub async fn session_set_config_option(
        &self,
        session_id: SessionId,
        option_id: String,
        value: String,
    ) -> Result<()> {
        let _: SetConfigOptionResult = self
            .call(
                "session/set_config_option",
                SetConfigOptionParams { session_id, option_id, value },
            )
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Convenience: send a plain-text prompt on the current session
    // ------------------------------------------------------------------

    /// Prompt the current session with a plain text message.
    /// Returns `Err` if no session has been created yet.
    pub async fn prompt_text(&self, text: impl Into<String>) -> Result<StopReason> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no active session — call session_new() first"))?;
        self.session_prompt(session_id, vec![ContentBlock::text(text)]).await
    }

    /// Create a lightweight cloneable handle for use in background tasks (Jobs).
    /// The handle shares the transport sender and request counter with this client.
    pub fn handle(&self) -> ClientHandle {
        ClientHandle {
            id: self.id,
            name: self.name.clone(),
            server_tx: self.server_tx.clone(),
            request_counter: Arc::clone(&self.request_counter),
        }
    }


    /// Concatenate all [`DisplayLine::Text`] entries into a single string.
    /// Used for tests and for producing a plain-text fallback.
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
/// Shares the transport sender and request counter with the owning `Client`.
#[derive(Clone)]
pub struct ClientHandle {
    pub id: AgentId,
    name: String,
    server_tx: UnboundedSender<Payload>,
    request_counter: Arc<AtomicU64>,
}

impl ClientHandle {
    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a request and await the response, deserializing it into `R`.
    pub async fn call<R: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<R> {
        let id = self.next_request_id();

        let params_value = serde_json::to_value(&params)?;
        let params = match params_value {
            Value::Object(map) => jsonrpc::Params::Map(map),
            Value::Array(arr) => jsonrpc::Params::Array(arr),
            Value::Null => jsonrpc::Params::None,
            other => jsonrpc::Params::Array(vec![other]),
        };

        let method_call = jsonrpc::MethodCall {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_owned(),
            params,
            id: jsonrpc::Id::Num(id),
        };

        let (tx, mut rx) = channel(1);
        self.server_tx
            .send(Payload::Request {
                chan: tx,
                value: method_call,
            })
            .map_err(|_| Error::StreamClosed)?;

        let value = rx.recv().await.ok_or(Error::StreamClosed)??;
        serde_json::from_value(value).map_err(Into::into)
    }

    /// Send a fire-and-forget notification (no response expected).
    fn notify(&self, method: &str, params: impl Serialize) -> Result<()> {
        let params_value = serde_json::to_value(&params)?;
        let params = match params_value {
            Value::Object(map) => jsonrpc::Params::Map(map),
            Value::Array(arr) => jsonrpc::Params::Array(arr),
            Value::Null => jsonrpc::Params::None,
            other => jsonrpc::Params::Array(vec![other]),
        };
        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_owned(),
            params,
        };
        self.server_tx
            .send(Payload::Notification(notification))
            .map_err(|_| Error::StreamClosed)?;
        Ok(())
    }

    /// Run the initialize handshake. Returns the result to store on the Client.
    pub async fn initialize(&self) -> Result<InitializeResult> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities {
                filesystem: Some(FileSystemCapabilities {
                    read_text_file: true,
                    write_text_file: true,
                }),
                terminal: Some(TerminalCapabilities::default()),
            },
            client_info: Some(ClientInfo {
                name: "helix".to_owned(),
                title: Some("Helix Editor".to_owned()),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        };
        let result: InitializeResult = self.call("initialize", params).await?;
        log::info!(
            "ACP agent '{}' initialized (protocol_version={})",
            self.name,
            result.protocol_version
        );
        Ok(result)
    }

    /// Create a new conversation session. Returns the result to store on the Client.
    pub async fn session_new(&self) -> Result<NewSessionResult> {
        let result: NewSessionResult = self
            .call("session/new", NewSessionParams::default())
            .await?;
        log::info!("ACP agent '{}' session created: {}", self.name, result.session_id);
        Ok(result)
    }

    /// Send a user prompt and wait for the agent's stop reason.
    pub async fn session_prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<StopReason> {
        let result: PromptResult = self
            .call("session/prompt", PromptParams { session_id, prompt })
            .await?;
        Ok(result.stop_reason)
    }

    /// Cancel an ongoing prompt turn (fire-and-forget notification).
    pub fn session_cancel(&self, session_id: SessionId) -> Result<()> {
        self.notify("session/cancel", CancelParams { session_id })
    }
}
