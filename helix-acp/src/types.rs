//! ACP protocol data types.
//!
//! All field names follow camelCase as required by the JSON specification for ACP.

use serde::{Deserialize, Serialize};

pub type ProtocolVersion = u16;
pub type SessionId = String;
pub type ToolCallId = String;

// ---------------------------------------------------------------------------
// Capability structures
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FileSystemCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalCapabilities>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    #[serde(default)]
    pub read_text_file: bool,
    #[serde(default)]
    pub write_text_file: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TerminalCapabilities {}

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
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

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
pub struct InitializeParams {
    pub protocol_version: ProtocolVersion,
    pub capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
}

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
// authenticate
// ---------------------------------------------------------------------------

/// Flexible authenticate params — the spec leaves the auth method open.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateParams {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AuthenticateResult {}

// ---------------------------------------------------------------------------
// session/new
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: SessionId,
}

// ---------------------------------------------------------------------------
// session/load
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    pub session_id: SessionId,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LoadSessionResult {}

// ---------------------------------------------------------------------------
// session/prompt
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: SessionId,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: StopReason,
}

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
// session/cancel  (notification, no response)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub session_id: SessionId,
}

// ---------------------------------------------------------------------------
// session/set_config_option
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: SessionId,
    pub option_id: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SetConfigOptionResult {}

// ---------------------------------------------------------------------------
// session/set_mode
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    pub session_id: SessionId,
    pub mode: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SetModeResult {}

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
// session/update notification (agent → client)
// ---------------------------------------------------------------------------

/// Params of the `session/update` notification sent by the agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    pub session_id: SessionId,
    pub update: SessionUpdate,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    UserMessageChunk {
        content: ContentBlock,
    },
    AgentMessageChunk {
        content: ContentBlock,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    ToolCall {
        id: ToolCallId,
        /// Tool name / type (e.g. "bash", "file_read").
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    ToolCallUpdate {
        id: ToolCallId,
        status: ToolCallStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
    },
    PlanUpdate {
        plan: Vec<PlanStep>,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands")]
        commands: Vec<SlashCommand>,
    },
    ConfigOptionUpdate {
        option: SessionConfigOption,
    },
    CurrentModeUpdate {
        mode: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Plan types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    pub status: PlanStepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Session config
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    pub id: String,
    pub label: String,
    pub groups: Vec<SessionConfigSelectGroup>,
    pub current_value: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectGroup {
    pub label: String,
    pub options: Vec<SessionConfigSelectOption>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    pub value: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// session/request_permission  (agent → client method call)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: SessionId,
    pub description: String,
    pub options: Vec<PermissionOption>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResult {
    pub outcome: PermissionOptionKind,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub kind: PermissionOptionKind,
    pub label: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

// ---------------------------------------------------------------------------
// fs/read_text_file  (agent → client method call)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileParams {
    /// Absolute path, as required by the ACP spec.
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileResult {
    pub content: String,
}

// ---------------------------------------------------------------------------
// fs/write_text_file  (agent → client method call)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WriteTextFileParams {
    /// Absolute path, as required by the ACP spec.
    pub path: String,
    pub content: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WriteTextFileResult {}

// ---------------------------------------------------------------------------
// terminal/*  (agent → client method calls)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CreateTerminalParams {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CreateTerminalResult {
    pub terminal_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputParams {
    pub terminal_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputResult {
    pub output: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct KillTerminalParams {
    pub terminal_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct KillTerminalResult {}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseTerminalParams {
    pub terminal_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ReleaseTerminalResult {}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WaitForExitParams {
    pub terminal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WaitForExitResult {
    pub exit_code: Option<i32>,
}
