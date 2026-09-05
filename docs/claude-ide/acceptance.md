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
| 16 | Second `claude --ide` in the same workspace | Helix log: `disconnecting previous WebSocket client`; the two CLIs then evict each other for a few seconds because the CLI auto-reconnects — run one CLI per workspace |
| 17 | `:q!` in Helix | lock file removed; the CLI reports the IDE as disconnected |
| 18 | `kill -9` Helix | lock file stays (observed with the example server); the CLI removes it on its next scan via the pid probe (PROTO §1.3, not observed) |

Automated equivalents: `cargo test -p helix-claude-ide`,
`cargo test -p helix-term --features integration --test claude_ide_integration`,
`cargo test -p helix-term --features integration --test claude_ide_split`.
