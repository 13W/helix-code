# Phase 3 — Prompt Turn

> Source: https://agentclientprotocol.com/protocol/prompt-turn

A **prompt turn** is a complete interaction cycle: the client sends a user message, the
agent processes it (potentially calling tools and requesting permissions), and the cycle
ends when the agent returns a `StopReason`.

---

## `session/prompt` — Send a User Message

### Request params

```json
{
  "sessionId": "sess_abc123def456",
  "content": [
    { "type": "text", "text": "Refactor this function to use async/await" },
    {
      "type": "resource",
      "resource": {
        "uri": "file:///workspace/src/main.rs",
        "mimeType": "text/x-rust"
      }
    }
  ]
}
```

Content blocks in the prompt are gated by capabilities declared during initialization:

| Block type | Capability required |
|------------|---------------------|
| `text` | Always available (baseline) |
| `image` | `promptCapabilities.image` |
| `audio` | `promptCapabilities.audio` |
| `resource` / `embedded_context` | `promptCapabilities.embeddedContext` |

---

## Response Flow

The `session/prompt` response arrives **after** all `session/update` notifications.

```
Client                            Agent
  |                                 |
  |--- session/prompt ------------->|
  |<-- session/update (text chunk) -|  (streaming text)
  |<-- session/update (tool call)  -|  (tool invocation announced)
  |--- session/request_permission ->|  (agent asks user)
  |<-- permission response ---------|
  |<-- session/update (tool result)-|  (tool completed)
  |<-- session/update (text chunk) -|  (more text)
  |<-- PromptResult --------------  |  { stopReason: "end_turn" }
```

---

## `session/update` Notification

Sent by the agent to stream progress. A single turn can produce many updates.

Update payload types:

| Type | Description |
|------|-------------|
| `text` | A chunk of generated text |
| `plan` | Task list with priorities (planning information) |
| `tool_call` | Tool invocation announcement (`in_progress`) |
| `tool_result` | Tool execution completed (success or error) |
| `metadata` | Session metadata updates (e.g., title change) |

---

## Tool Call Lifecycle

When the LLM requests a tool, the sequence is:

1. Agent emits `session/update` with `type: "tool_call"`, status `in_progress`
2. If the operation requires user approval, agent calls `session/request_permission`
3. Client responds with approval or denial
4. Agent executes the tool (file read/write, terminal command, etc.)
5. Agent emits `session/update` with `type: "tool_result"` (success or error)
6. Result is fed back to the LLM; the turn continues

---

## `session/request_permission`

Called by the **agent** on the **client** when user authorization is required.

```json
{
  "sessionId": "sess_abc123def456",
  "toolCallId": "tool_xyz789",
  "description": "Write to /workspace/src/main.rs",
  "operations": [
    { "type": "fs_write", "path": "/workspace/src/main.rs" }
  ]
}
```

The client presents this to the user and responds with `approved: true` or `false`.

---

## Stop Reasons

The `session/prompt` response includes a `stopReason` indicating why the turn ended.

| Value | Description |
|-------|-------------|
| `end_turn` | Agent completed its response normally |
| `max_tokens` | Token limit reached |
| `max_turn_requests` | Too many tool calls in one turn |
| `refusal` | Agent refused to complete the request |
| `cancelled` | Turn was cancelled by `session/cancel` |

---

## `session/cancel` — Cancel a Turn

A **notification** (no response expected) sent by the client to interrupt processing.

```json
{
  "sessionId": "sess_abc123def456"
}
```

**Critical:** Agents MUST catch cancellation and return `stopReason: "cancelled"` — not
an error. This allows clients to distinguish intentional cancellations from actual
failures.

---

## Implementation Notes (Helix)

Relevant file: [`helix-acp/src/types.rs`](../../helix-acp/src/types.rs)

```rust
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}
```

Values are serialized as `snake_case` JSON strings (e.g., `"end_turn"`, `"max_tokens"`).
