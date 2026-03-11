# Agent Client Protocol (ACP) — Knowledge Base

> **Official specification:** https://agentclientprotocol.com/protocol/overview

ACP is a bidirectional JSON-RPC 2.0 protocol that connects AI agents (programs that
modify code using an LLM) with clients (IDEs, editors, UI frontends). It defines how
clients create sessions, send prompts, receive streaming updates, and grant permissions
for file system and terminal operations.

**Helix implementation:** [`helix-acp/src/`](../../helix-acp/src/)

---

## Contents

| File | Description |
|------|-------------|
| [overview.md](./overview.md) | Protocol fundamentals, message types, lifecycle phases, full method list |
| [initialization.md](./initialization.md) | Phase 1 — version negotiation and capability exchange |
| [session-setup.md](./session-setup.md) | Phase 2 — creating/loading sessions, MCP server transports |
| [prompt-turn.md](./prompt-turn.md) | Phase 3 — prompt/response cycle, tool calls, cancellation |
| [extensibility.md](./extensibility.md) | `_meta` field, custom extension methods, capability advertising |
| [schema.md](./schema.md) | All data types: ContentBlock, StopReason, Capabilities, SessionId, etc. |

---

## Quick Reference

### Three Lifecycle Phases

```
Client                          Agent
  |                               |
  |--- initialize -------------->|  (version + capabilities)
  |<-- InitializeResult ---------|
  |                               |
  |--- authenticate ------------>|  (credentials)
  |<-- AuthenticateResult -------|
  |                               |
  |--- session/new ------------->|  (cwd + MCP servers)
  |<-- { sessionId } ------------|
  |                               |
  |--- session/prompt ---------->|  (user message)
  |<-- session/update (N times) -|  (streaming progress)
  |<-- PromptResult -------------|  (stop reason)
```

### Agent Methods

| Method | Required | Description |
|--------|----------|-------------|
| `initialize` | Yes | Version negotiation and capability exchange |
| `authenticate` | Yes | Client authentication |
| `session/new` | Yes | Create a new session |
| `session/prompt` | Yes | Send a user message |
| `session/cancel` | Yes (notification) | Cancel ongoing processing |
| `session/load` | Optional | Resume a previous session |
| `session/list` | Optional | List existing sessions |
| `session/set_mode` | Optional | Switch agent operating mode |
| `session/set_config_option` | Optional | Configure agent settings |

### Client Methods (called by Agent)

| Method | Required | Description |
|--------|----------|-------------|
| `session/request_permission` | Yes | Request user authorization |
| `session/update` | Yes (notification) | Stream progress to client |
| `fs/read_text_file` | Optional | Read a file |
| `fs/write_text_file` | Optional | Write a file |
| `terminal/create` | Optional | Create a terminal |
| `terminal/output` | Optional | Send output to terminal |
| `terminal/kill` | Optional | Kill a terminal process |
| `terminal/wait_for_exit` | Optional | Wait for process exit |
| `terminal/release` | Optional | Release a terminal |
