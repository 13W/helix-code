# helix-claude-ide

The IDE side of the protocol that the `claude` CLI speaks to editors
(`claude --ide`, `/ide`): a loopback WebSocket MCP server discovered through
a lock file, four tools, and a handful of notifications. Helix embeds it via
`--claude-ide` / `[editor.claude-ide]`; the crate itself knows nothing about
the editor and delegates tool calls to a [`ToolHandler`](src/tools.rs).

## Protocol baseline

Everything here was implemented against
[`claude-code-ide-protocol-spec.md`](../claude-code-ide-protocol-spec.md),
which was extracted from the official binaries:

| Component | Version | Notes |
|---|---|---|
| `claude` CLI | **2.1.261** (build 2026-09-04, `1349cf9c`) | also the version used for every live check below |
| VS Code extension `anthropic.claude-code` | **2.1.261** | reference for tool schemas, lock file, `openDiff` semantics |
| MCP `protocolVersion` | `2025-11-25` (echoed; `2025-06-18`, `2025-03-26`, `2024-11-05`, `2024-10-07` accepted) | |

When the CLI is upgraded, re-check in this order:

1. **Discovery** – lock-file fields (`pid`, `workspaceFolders`, `ideName`,
   `transport`, `runningInWindows`, `authToken`), the
   `X-Claude-Code-Ide-Authorization` header, the `mcp` sub-protocol echo,
   the `cwd`-inside-workspace validity rule (`src/lockfile.rs`, `src/transport.rs`).
2. **Handshake** – `initialize` reply (`serverInfo.name` `Claude Code Helix MCP`,
   `capabilities.tools.listChanged`), `notifications/initialized`,
   `ide_connected` (`src/server.rs`).
3. **Tools the CLI actually calls** (`src/tools.rs`, `src/handler.rs`) —
   the only ones published:

   | Tool | When the CLI calls it | Reply |
   |---|---|---|
   | `closeAllDiffTabs` | at the start of every user turn | `CLOSED_<n>_DIFF_TABS` |
   | `getDiagnostics {uri}` / `{}` | baseline before an Edit/Write (500 ms budget) and right after it (2 s) | JSON array `[{uri, linesInFile?, diagnostics:[{message, severity, range, source?, code?}]}]` |
   | `openDiff {old_file_path, new_file_path, new_file_contents, tab_name}` | only while asking permission for Edit/Write (permission mode `default`) with `diffTool == "auto"` (global config default) | `["FILE_SAVED", <contents>]` or `["DIFF_REJECTED", <tab_name>]`; blocks until decided |
   | `close_tab {tab_name}` | after the terminal prompt was answered, on abort, on exit | always `TAB_CLOSED` |
   | `set_permission_mode {mode}` | when the permission mode changes (**not in the spec**; errors are swallowed) | `-32602 Tool not found` — harmless |

   Tools the extension registers but the CLI never calls (`openFile`,
   `getOpenEditors`, `getCurrentSelection`, `getLatestSelection`,
   `checkDocumentDirty`, `saveDocument`, `getWorkspaceFolders`, `executeCode`)
   are deliberately not implemented.
4. **Notifications IDE → CLI** (`src/notify.rs`): `selection_changed`
   (300 ms trailing debounce, de-duplicated, cached value replayed 500 ms after
   a client connects) and `at_mentioned` (`:claude-mention`).

## Observed CLI behaviour that differs from the spec

- The CLI **auto-reconnects** after the WebSocket closes (backoff 1 s, 2 s, …);
  the spec says it does not. Two CLIs started in the same workspace therefore
  evict each other back and forth for a while (the server keeps one client).
- `openDiff` is part of the *permission* flow, so it never appears in
  `acceptEdits`, `plan` or `bypassPermissions` modes.
- The CLI calls `set_permission_mode`, which the spec does not list.
- `claude -p` (print mode) never connects to an IDE.

## Layout

| File | Role |
|---|---|
| `src/lockfile.rs` | `<configDir>/ide/<port>.lock` (0700 / 0600) |
| `src/port.rs` | random port in `10000..=65535`, ≤ 50 bind attempts |
| `src/transport.rs` | axum WebSocket server, auth, single client, one JSON-RPC message per frame, each request on its own task |
| `src/jsonrpc.rs` | minimal JSON-RPC 2.0 types (no MCP SDK) |
| `src/server.rs` | `initialize`, `ping`, `tools/list`, `tools/call` dispatch |
| `src/tools.rs` | published tool schemas, argument validation, `ToolHandler` trait |
| `src/handler.rs` | `EditorHandler`: forwards tools to Helix over the `McpCommand` channel |
| `src/diff.rs` | registry of pending `openDiff` proposals |
| `src/diagnostics.rs` | `file://` URI ↔ path, VS Code-shaped diagnostics JSON |
| `src/notify.rs` | `selection_changed` / `at_mentioned` |
| `examples/serve.rs` | stand-alone server for interoperability checks (`claude --ide` against it) |
| `examples/client.rs` | CLI-side client: `client <port.lock> getDiagnostics '{"uri":"file:///…"}'` |

## Tests

```sh
cargo test -p helix-claude-ide                                   # unit + WebSocket-level tests
cargo test -p helix-term --features integration --test claude_ide_integration
cargo test -p helix-term --features integration --test claude_ide_split
```

The manual acceptance script lives in [`docs/claude-ide/acceptance.md`](../docs/claude-ide/acceptance.md).
