# Phase 2 — Session Setup

> Source: https://agentclientprotocol.com/protocol/session-setup

A **session** represents a single conversation thread between a client and an agent.
Each session has its own independent context and message history. Multiple sessions can
exist simultaneously.

**Pre-condition:** initialization (and authentication) must be complete before creating
any session.

---

## `session/new` — Create a Session

### Request params

```json
{
  "cwd": "/absolute/path/to/workspace",
  "mcpServers": [
    {
      "type": "stdio",
      "name": "filesystem",
      "command": "/usr/bin/mcp-server-filesystem",
      "args": ["--root", "/workspace"],
      "env": { "LOG_LEVEL": "info" }
    }
  ]
}
```

### Response

```json
{
  "sessionId": "sess_abc123def456",
  "configOptions": [
    {
      "key": "model",
      "name": "Model",
      "value": "claude-opus-4",
      "options": [...]
    }
  ]
}
```

- `cwd` **must be an absolute path**
- `sessionId` is the unique identifier used in all subsequent calls
- `configOptions` contains agent-specific settings (model choice, mode, etc.)

---

## `session/load` — Resume an Existing Session

Requires the agent to have declared `loadSession: true` during initialization.

### Request params

```json
{
  "sessionId": "sess_abc123def456",
  "cwd": "/absolute/path/to/workspace",
  "mcpServers": [...]
}
```

### Behavior

The agent **replays the entire conversation history** as a series of `session/update`
notifications before returning the load result. The client should render these as the
existing conversation context.

---

## `session/list` — List Sessions

Requires `sessionCapabilities.list` in the agent's capabilities.

Returns a list of sessions with their IDs and metadata (title, last modified, etc.).

---

## MCP Server Transports

MCP servers are external tools that the agent can use during a session. They are
configured per-session in `session/new` and `session/load`.

### Stdio (Required — all agents must support)

```json
{
  "type": "stdio",
  "name": "my-server",
  "command": "/path/to/mcp-server",
  "args": ["--flag", "value"],
  "env": { "API_KEY": "secret" }
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | Always `"stdio"` |
| `name` | Yes | Identifier for this server |
| `command` | Yes | Executable path (absolute) |
| `args` | No | Command-line arguments |
| `env` | No | Additional environment variables |

### HTTP (Optional — requires `mcpCapabilities.http`)

```json
{
  "type": "http",
  "name": "remote-server",
  "url": "https://mcp.example.com/",
  "headers": {
    "Authorization": "Bearer token123"
  }
}
```

### SSE — Server-Sent Events (Deprecated)

```json
{
  "type": "sse",
  "name": "sse-server",
  "url": "https://mcp.example.com/sse",
  "headers": {}
}
```

SSE is deprecated by the MCP specification. Prefer HTTP transport. Requires
`mcpCapabilities.sse` during initialization before attempting to use this transport.

---

## Working Directory

The `cwd` field establishes the **operational context** for the session. All relative
paths used during the session are resolved against this directory. It must be an
absolute file system path.
