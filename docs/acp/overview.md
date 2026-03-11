# ACP Overview

> Source: https://agentclientprotocol.com/protocol/overview

## What is ACP?

The Agent Client Protocol (ACP) defines how AI-powered coding agents communicate with
editor clients (IDEs, terminals, UI frontends). It is built on **JSON-RPC 2.0** and
supports bidirectional communication — both sides can initiate requests and send
notifications.

**Agent** — an AI-powered program that can read and modify code (e.g., Claude Code,
Zed AI, Cursor).

**Client** — a user-facing interface that connects to an agent to provide AI features
(e.g., Helix editor with the ACP integration).

---

## Message Types

ACP uses two message categories from JSON-RPC 2.0:

| Type | Has `id` | Expects response | Description |
|------|----------|-----------------|-------------|
| **Method** (request) | Yes | Yes | Request/response pair |
| **Notification** | No | No | One-way fire-and-forget |

---

## Three Lifecycle Phases

Every ACP interaction follows three phases in order:

### Phase 1 — Initialization
The client calls `initialize` to negotiate protocol version and exchange capability
declarations. Authentication (`authenticate`) follows immediately after.

No sessions can be created before this phase completes.

See [initialization.md](./initialization.md).

### Phase 2 — Session Setup
The client creates a session with `session/new` (or resumes one with `session/load`).
A session is an independent conversation thread with its own context and history.

See [session-setup.md](./session-setup.md).

### Phase 3 — Prompt Turn
The client sends user messages via `session/prompt`. The agent streams progress back
via `session/update` notifications and eventually returns a `StopReason`.

See [prompt-turn.md](./prompt-turn.md).

---

## Agent Methods (full list)

### Baseline (required)

| Method | Type | Description |
|--------|------|-------------|
| `initialize` | Request | Protocol version negotiation and capability exchange |
| `authenticate` | Request | Client authentication |
| `session/new` | Request | Create a new conversation session |
| `session/prompt` | Request | Transmit a user message and receive the response |
| `session/cancel` | Notification | Interrupt ongoing processing |

### Optional

| Method | Type | Requires capability | Description |
|--------|------|---------------------|-------------|
| `session/load` | Request | `loadSession` | Resume a previous session |
| `session/list` | Request | `sessionCapabilities.list` | Enumerate existing sessions |
| `session/set_mode` | Request | — | Switch agent operating mode |
| `session/set_config_option` | Request | — | Configure agent settings |

---

## Client Methods (full list)

Methods called by the **agent** on the **client**.

### Baseline (required)

| Method | Type | Description |
|--------|------|-------------|
| `session/request_permission` | Request | Request user authorization for an action |
| `session/update` | Notification | Stream progress, tool calls, and text chunks |

### Optional — File System

| Method | Type | Requires capability | Description |
|--------|------|---------------------|-------------|
| `fs/read_text_file` | Request | `fileSystem` | Read file contents |
| `fs/write_text_file` | Request | `fileSystem` | Write file contents |

All file paths **MUST** be absolute. Line numbers are **1-based**.

### Optional — Terminal

| Method | Type | Requires capability | Description |
|--------|------|---------------------|-------------|
| `terminal/create` | Request | `terminal` | Create a new terminal instance |
| `terminal/output` | Notification | `terminal` | Send output to terminal |
| `terminal/kill` | Request | `terminal` | Kill the running process |
| `terminal/wait_for_exit` | Request | `terminal` | Block until process exits |
| `terminal/release` | Request | `terminal` | Release the terminal |

---

## Key Constraints

- All file paths must be **absolute**
- Line numbers are **1-based**
- Standard JSON-RPC 2.0 error handling applies
- Clients MUST complete initialization before creating sessions
- Agents MUST catch cancellation and return `cancelled` stop reason (not an error)
