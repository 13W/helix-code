//! `helix-acp-types` — pure data types for the Agent Client Protocol (ACP) integration.
//!
//! This crate contains only serializable DTOs, enums, and lightweight value types.
//! It has no dependency on the `agent-client-protocol` SDK, tokio, or axum, so it
//! can be used by any crate that needs to inspect or render ACP data without
//! pulling in the full agent runtime.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Identity & result types
// ---------------------------------------------------------------------------

/// Opaque identifier for a running ACP agent.
///
/// Constructed only by [`helix_acp::Registry`]; not directly constructable by users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(pub u64);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent#{}", self.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    ParseError(String),

    #[error("stream closed")]
    StreamClosed,

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::ParseError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Protocol type aliases
// ---------------------------------------------------------------------------

pub type ProtocolVersion = u16;
pub type SessionId = String;
pub type ToolCallId = String;

// ---------------------------------------------------------------------------
// Capability structures
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<PromptCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_capabilities: Option<McpCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub embedded_context: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    #[serde(default)]
    pub http: bool,
    #[serde(default)]
    pub sse: bool,
}

/// One authentication method the agent supports.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionForkCapabilities {}
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionListCapabilities {}
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionResumeCapabilities {}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<SessionForkCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionResumeCapabilities>,
}

// ---------------------------------------------------------------------------
// Implementation info
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: ProtocolVersion,
    #[serde(rename = "agentCapabilities")]
    pub capabilities: AgentCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<AgentInfo>,
    /// Auth methods the agent accepts (may be empty if no auth required).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_methods: Vec<AuthMethod>,
}

// ---------------------------------------------------------------------------
// authenticate / account
// ---------------------------------------------------------------------------

/// Flexible authenticate params — the spec leaves the auth method open.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateParams {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Account information returned by the ACP `account/info` extension method.
#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub name: Option<String>,
    pub account_uuid: Option<String>,
}

// ---------------------------------------------------------------------------
// session/prompt
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Content types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mediaType")]
        media_type: String,
    },
    Audio {
        data: String,
        #[serde(rename = "mediaType")]
        media_type: String,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
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
// Display / UI types
// ---------------------------------------------------------------------------

/// A session entry returned by the ACP `session/list` extension method.
#[derive(Debug, Clone)]
pub struct ListedSession {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at: String,
}

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

/// Accumulated token and cost statistics for the current session.
#[derive(Debug, Default)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens served from the prompt cache (cachedReadTokens).
    pub cache_read_tokens: u64,
    /// Tokens written to the prompt cache (cachedWriteTokens).
    pub cache_write_tokens: u64,
    pub cost_amount: f64,
    pub currency: String,
    /// Context window tokens used (from UsageUpdate).
    pub context_used: u64,
    /// Context window total size (from UsageUpdate).
    pub context_size: u64,
}
