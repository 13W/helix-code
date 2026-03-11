# Phase 1 — Initialization

> Source: https://agentclientprotocol.com/protocol/initialization

The initialization phase establishes a connection between the client and agent. It MUST
complete before any session can be created. The client always initiates.

---

## `initialize`

The client sends its protocol version and declared capabilities. The agent responds with
the negotiated protocol version and its own capabilities.

### Request params

```json
{
  "protocolVersion": 1,
  "clientInfo": {
    "name": "helix",
    "title": "Helix Editor",
    "version": "25.1.0"
  },
  "clientCapabilities": {
    "fileSystem": {
      "readTextFile": true,
      "writeTextFile": true
    },
    "terminal": {}
  }
}
```

### Response

```json
{
  "protocolVersion": 1,
  "agentInfo": {
    "name": "claude-code",
    "title": "Claude Code",
    "version": "1.0.0"
  },
  "agentCapabilities": {
    "loadSession": true,
    "promptCapabilities": {
      "image": true,
      "audio": false,
      "embeddedContext": true
    },
    "mcpCapabilities": {
      "http": true,
      "sse": false
    },
    "sessionCapabilities": {
      "list": {},
      "fork": {},
      "resume": {}
    }
  },
  "authMethods": [
    {
      "id": "api_key",
      "name": "API Key",
      "description": "Authenticate with an API key"
    }
  ]
}
```

---

## Protocol Version Negotiation

- Version is a **single integer** — incremented only on MAJOR breaking changes
- The client sends its **latest supported version**
- The agent responds with that version if supported, or its own latest version
- If the versions are **incompatible**, both sides should disconnect

---

## Client Capabilities

Declared by the client; agents check these before calling client-side methods.

| Field | Type | Description |
|-------|------|-------------|
| `fileSystem.readTextFile` | bool | Client supports `fs/read_text_file` |
| `fileSystem.writeTextFile` | bool | Client supports `fs/write_text_file` |
| `terminal` | object | Client supports terminal management methods |

---

## Agent Capabilities

Declared by the agent; clients check these before calling optional agent methods.

| Field | Type | Description |
|-------|------|-------------|
| `loadSession` | bool | Agent supports `session/load` |
| `promptCapabilities.text` | implicit | Always supported (baseline) |
| `promptCapabilities.image` | bool | Agent can process image content blocks |
| `promptCapabilities.audio` | bool | Agent can process audio content blocks |
| `promptCapabilities.embeddedContext` | bool | Agent can process embedded context blocks |
| `mcpCapabilities.http` | bool | Agent supports HTTP MCP transport |
| `mcpCapabilities.sse` | bool | Agent supports SSE MCP transport (deprecated) |
| `sessionCapabilities.list` | object | Agent supports `session/list` |
| `sessionCapabilities.fork` | object | Agent supports session forking |
| `sessionCapabilities.resume` | object | Agent supports session resume |

---

## Authentication

After `initialize`, the client calls `authenticate` before creating any session.

The set of accepted authentication methods is declared in `authMethods` of the
`initialize` response. The content of the authenticate params is method-specific and
intentionally open (the spec does not mandate a specific auth scheme).

```json
{
  "method": "api_key",
  "apiKey": "sk-..."
}
```

The Helix implementation in `helix-acp/src/types.rs` uses a flat `extra` map to accept
any authentication params without a fixed schema:

```rust
pub struct AuthenticateParams {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
```

---

## Implementation Notes (Helix)

Relevant file: [`helix-acp/src/types.rs`](../../helix-acp/src/types.rs)

Key types:
- `InitializeResult` — agent's response to `initialize`
- `AgentCapabilities` — all optional fields, `skip_serializing_if = "Option::is_none"`
- `AuthMethod` — `{ id, name, description? }`
- `AgentInfo` — `{ name, title?, version? }`
- All JSON field names are **camelCase** (enforced via `#[serde(rename_all = "camelCase")]`)
