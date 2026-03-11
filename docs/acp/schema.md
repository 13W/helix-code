# ACP Schema Reference

> Source: https://agentclientprotocol.com/protocol/schema

All JSON field names in ACP use **camelCase**. In the Helix Rust implementation this is
enforced via `#[serde(rename_all = "camelCase")]`.

---

## Primitive Types

| Type | Rust | JSON | Example |
|------|------|------|---------|
| `SessionId` | `String` | string | `"sess_abc123def456"` |
| `ProtocolVersion` | `u16` | number | `1` |
| `ToolCallId` | `String` | string | `"tool_xyz789"` |

---

## ContentBlock

A union type used in `session/prompt` content and `session/update` streaming.

```rust
pub enum ContentBlock {
    Text { text: String },
    Image { data: String, media_type: String },  // JSON: mediaType
    Audio { data: String, media_type: String },  // JSON: mediaType
}
```

JSON representations:

```json
{ "type": "text", "text": "Hello, world!" }

{ "type": "image", "data": "<base64>", "mediaType": "image/png" }

{ "type": "audio", "data": "<base64>", "mediaType": "audio/mp3" }
```

Type is gated by `promptCapabilities` declared during initialization.

---

## StopReason

Returned in the `session/prompt` response to indicate why a turn ended.

```rust
pub enum StopReason {
    EndTurn,          // JSON: "end_turn"
    MaxTokens,        // JSON: "max_tokens"
    MaxTurnRequests,  // JSON: "max_turn_requests"
    Refusal,          // JSON: "refusal"
    Cancelled,        // JSON: "cancelled"
}
```

---

## AgentInfo / ClientInfo

Implementation identification for debugging and logging.

```rust
pub struct AgentInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: Option<String>,
}
```

---

## AuthMethod

Describes an authentication option offered by the agent.

```rust
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}
```

---

## AgentCapabilities

All fields are optional. Absence means the feature is not supported.

```rust
pub struct AgentCapabilities {
    pub load_session: Option<bool>,               // JSON: loadSession
    pub prompt_capabilities: Option<PromptCapabilities>,
    pub mcp_capabilities: Option<McpCapabilities>,
    pub session_capabilities: Option<SessionCapabilities>,
}
```

---

## PromptCapabilities

Declares which content block types the agent can process.

```rust
pub struct PromptCapabilities {
    pub audio: bool,
    pub image: bool,
    pub embedded_context: bool,  // JSON: embeddedContext
}
```

---

## McpCapabilities

Declares which MCP transport protocols the agent supports.

```rust
pub struct McpCapabilities {
    pub http: bool,
    pub sse: bool,  // deprecated transport
}
```

---

## SessionCapabilities

Declares optional session management features.

```rust
pub struct SessionCapabilities {
    pub fork: Option<SessionForkCapabilities>,
    pub list: Option<SessionListCapabilities>,
    pub resume: Option<SessionResumeCapabilities>,
}
```

---

## AccountInfo (Extension)

Custom extension type returned by the `account/info` method (Helix-specific).

```rust
pub struct AccountInfo {
    pub email: Option<String>,
    pub name: Option<String>,
    pub account_uuid: Option<String>,
}
```

---

## NewSessionResult

Response from `session/new`.

```rust
pub struct NewSessionResult {
    pub session_id: SessionId,
    pub config_options: Vec<SessionConfigOption>,  // from agent-client-protocol SDK
}
```

`SessionConfigOption` contains model/mode selections that the user can configure.

---

## InitializeResult

Response from `initialize`.

```rust
pub struct InitializeResult {
    pub protocol_version: ProtocolVersion,
    pub capabilities: AgentCapabilities,     // JSON field: "agentCapabilities"
    pub agent_info: Option<AgentInfo>,
    pub auth_methods: Vec<AuthMethod>,
}
```

---

## Serialization Notes

- All struct field names use **camelCase** in JSON
- `StopReason` values use **snake_case** in JSON (e.g., `"end_turn"`)
- Optional fields with `None` are omitted from the serialized output
  (`skip_serializing_if = "Option::is_none"`)
- Empty `Vec` fields like `auth_methods` are omitted when empty
  (`skip_serializing_if = "Vec::is_empty"`)
