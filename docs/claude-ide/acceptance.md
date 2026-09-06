# Claude Code IDE integration — manual acceptance script

Protocol reference: [`claude-code-ide-protocol-spec.md`](../../claude-code-ide-protocol-spec.md)
(CLI / VS Code extension 2.1.261). Every step below was executed against the
real `claude` 2.1.261 on 2026-09-05 (macOS, clangd for diagnostics); the
"Expected" column is what was observed.

Setup: `cargo build -p helix-term`, a workspace with a `.git` marker, a file
with a language server (e.g. `main.c` + clangd, or a Cargo project with
rust-analyzer). Use `--debug-file <path>` on the CLI to see its side
(`MCP server "ide": Calling MCP tool: …`). `claude -p` never connects to an
IDE — all checks are interactive. Start the CLI **inside** the workspace.

| # | Step | Expected |
|---|---|---|
| 1 | `hx --claude-ide src/main.c` | stderr before the TUI: `helix-claude-ide: IDE server listening at ws://127.0.0.1:<port> (lock file ~/.claude/ide/<port>.lock)`; `cat ~/.claude/ide/<port>.lock` shows `pid` = Helix pid, `workspaceFolders` = workspace root, `ideName` = directory name, `transport: "ws"`, `authToken` (uuid); file mode 0600, dir 0700 |
| 2 | In another terminal (same directory): `claude --ide` | CLI status shows the IDE as connected; Helix `:claude-ide-status` → `client connected · pending diffs 0`; statusline `claude-ide-indicator` shows `✻◆` |
| 3 | `/ide` inside the CLI | Helix listed by name and marked connected (per PROTO §1.3–1.5; the picker itself was not driven in the automated runs, the connection was verified through the Helix log and `:claude-ide-status`) |
| 4 | In Helix: `x` `x` (select two lines) | CLI footer shows the selection ("1 line selected" then "2 lines") within ~300 ms |
| 5 | `:claude-mention` | CLI prompt gains `@main.c#L1-2 ` (`@main.c` for a bare cursor) |
| 6 | Diagnostics: open a file with an error, wait for the language server, then `cargo run -p helix-claude-ide --example client -- ~/.claude/ide/<port>.lock getDiagnostics '{"uri":"file:///abs/main.c"}'` | one entry, `uri` echoed verbatim, `linesInFile`, diagnostics with `severity: "Error"`, `range`, `source`, `code` |
| 7 | In the CLI (permission mode `default`): "insert `int z = other_missing;` after the line with `undefined_symbol` in main.c" | Helix shows **Claude Code proposes changes to main.c** with a unified diff (`diff-mode = "prompt"`); CLI shows "Opened changes in `<name>` ⧉"; Helix log: `getDiagnostics {uri}` (baseline) is *not* yet called |
| 8 | `Enter` (Apply) | CLI receives `FILE_SAVED`, calls `close_tab`, writes the file, then `getDiagnostics {uri}` and `getDiagnostics {}`; Helix reloads the buffer and shows the new line |
| 9 | Repeat 7, choose `Reject` (`↓` `Enter` or `Esc`) | CLI: "Edit tool permission denied"; file unchanged |
| 10 | `[editor.claude-ide] diff-mode = "split"`, repeat 7 | Three windows: original, left (read-only, gutter marks the changed lines), right `✻ main.c` with the proposal and `▍` on added lines; status line names `:claude-diff-accept` / `:claude-diff-reject` |
| 11 | Edit the right buffer, `:claude-diff-accept` (or `:w` there) | `FILE_SAVED` with the *edited* text; CLI writes exactly that; split closed, left file writable again, no `✻ main.c` file on disk |
| 12 | Repeat 10, `:bc` on the right buffer | `DIFF_REJECTED`; `:q` instead only closes the window and `:claude-ide-status` still shows `pending diffs 1` |
| 13 | Answer the CLI's terminal prompt instead of Helix | CLI sends `close_tab`; the Helix prompt/split disappears |
| 14 | `hx --mcp --claude-ide` | both `helix-mcp: MCP server listening at http://…/mcp` and the IDE server line; both work at once |
| 15 | Second Helix in another workspace, `claude --ide` in the first workspace | only the matching Helix gets `client #1 connected` (observed); the other one appears in `/ide` under "Found N other running IDE(s)" per PROTO §1.3 (not driven visually) |
| 16 | Second `claude --ide` in the same workspace | Both CLIs stay connected (T8): Helix log `client #2 connected (2 of 4)`, no eviction, no `attempting automatic reconnection` in either CLI's debug log; `/ide` shows Connected in both |
| 17 | `:q!` in Helix | lock file removed; the CLI reports the IDE as disconnected |
| 18 | `kill -9` Helix | lock file stays (observed with the example server); the CLI removes it on its next scan via the pid probe (PROTO §1.3, not observed) |

## T8 — several CLIs on one Helix

Setup as above, `diff-mode = "prompt"` unless stated; two terminals A and B in
the same workspace, both `claude --ide --permission-mode default --debug-file <path>`.
Status (2026-09-06, CLI 2.1.261): steps 19, 25 and 27 were executed against the
real CLI using the stand-alone server (`cargo run -p helix-claude-ide --example
serve`, `MAX_CLIENTS=1` for step 25) driven through a pty; the observations are
recorded in the "Expected" column. Steps 20–24, 26 and 28 need the full Helix
UI and were **not driven live** — they are covered by the automated tests
listed below (`claude_ide_multi`, `handshake`, `diff`).

| # | Step | Expected |
|---|---|---|
| 19 | Start A and B | **Observed** (example server): `client #1 connected (1 of 4)` … `client #1 is claude pid 27673`, then `client #2 connected (2 of 4)` … `pid 27765`; both CLIs log `MCP server "ide": Successfully connected (transport: ws-ide)`; no eviction, neither debug log contains `attempting automatic reconnection`. In Helix: `:claude-ide-status` lists both pids, statusline `✻◆2` (automated: `claude_ide_multi`) |
| 20 | In A ask for an edit; do not answer. In B ask for an edit | prompt "Claude Code (pid A) proposes changes to …"; B's prompt waits in the queue (`:claude-ide-status` shows `diffs 1` for each) |
| 21 | Apply A's prompt | A writes the file; B's prompt appears with "(pid B)" |
| 22 | Start a new turn in A (it sends `closeAllDiffTabs`) | B's prompt stays; A's debug log shows `CLOSED_0_DIFF_TABS` |
| 23 | `:claude-mention` with two CLIs and no focus | picker with `client / pid / mode / cwd`; choosing B inserts `@file#L…` into B only |
| 24 | `:claude-ide-focus <pid A>` then `:claude-mention` | mention goes to A without a picker; `:claude-ide-status` marks A with `●` |
| 25 | `max-clients = 2` in config, start a third and a fourth `claude --ide` | **Observed** with `MAX_CLIENTS=1` and a second CLI: server `[WARN] refusing WebSocket client 127.0.0.1:54193: too many clients (max-clients = 1)`; the refused CLI logs `MCP server "ide": Connection failed after 13ms: WebSocket connection to 'ws://127.0.0.1:37920/' failed: Expected 101 status code` and `[ERROR] MCP server "ide" Connection failed` — a **single attempt, no retries** within 45 s (the auto-reconnect of PROTO §2.6 only applies to a connection that was established and then closed); the first CLI is unaffected |
| 26 | `diff-mode = "split"`, edits from A and B on two different files | two splits side by side, right buffers `✻ <file> [<pid A>]` and `✻ <file> [<pid B>]`; `:claude-diff-accept` in one leaves the other pending |
| 27 | `/exit` in A while B has a pending proposal | **Observed** (example server, no pending proposals): A closes with 1000, server `client #1 disconnected (1 left)`, B stays connected and silent. Rejection of A's own proposals only and statusline `✻◆`: automated (`disconnect_rejects_only_own`, `claude_ide_multi`) |
| 28 | `:claude-ide-disconnect <pid B>` | B's socket closed with 1000 "Closed by user"; B reconnects on its own within seconds (expected, PROTO §2.6 as observed) |

Automated equivalents: `cargo test -p helix-claude-ide`,
`cargo test -p helix-term --features integration --test claude_ide_integration`,
`cargo test -p helix-term --features integration --test claude_ide_split`,
`cargo test -p helix-term --features integration --test claude_ide_multi`.
