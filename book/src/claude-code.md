# Claude Code integration

Helix can act as the editor for the [Claude Code](https://claude.com/claude-code)
CLI the same way the VS Code and JetBrains plugins do: the CLI connects to a
small server inside Helix and uses it to

- ask you to **review proposed edits** inside Helix before they are written,
- read **LSP diagnostics** from Helix after it edits a file,
- see your **current selection** ("N lines selected in `file`") and take
  `@file#L10-15` mentions from `:claude-mention`.

Requirements: `claude` CLI 2.1.261 or later, started **from the workspace
directory** (or a directory below it) of the Helix instance it should use.

## Starting the server

Either per launch:

```sh
hx --claude-ide                    # random port, name = workspace directory
hx --claude-ide-port 45000         # fixed port (implies --claude-ide)
hx --claude-ide-name backend       # name shown in the CLI's /ide picker
```

or permanently, in `config.toml`:

```toml
[editor.claude-ide]
enable = true
```

or at runtime with `:claude-ide-start [port]`. `:claude-ide-status` shows the
port, the lock file, whether a CLI is connected and how many proposals are
waiting; `:claude-ide-stop` stops the server and removes the lock file. The
statusline element `claude-ide-indicator` shows `✻` while the server runs and
`✻◆` once a CLI is connected.

Then, in a terminal whose current directory is inside the workspace:

```sh
claude --ide          # connect at start-up
```

or type `/ide` inside a running `claude` session and pick the Helix entry.
The CLI finds Helix through `~/.claude/ide/<port>.lock` (honouring
`CLAUDE_CONFIG_DIR`); the lock file is removed when Helix exits.

## Reviewing proposals

When the CLI wants to edit or create a file and asks for permission (its
default permission mode), the proposal is shown in Helix according to
`diff-mode`:

- **`prompt`** (default): a dialog with a unified diff preview and
  `Apply` / `Reject`. `Enter` applies, `Escape` rejects.
- **`split`**: a vertical split with the current file on the left (read-only
  while the proposal is open; the gutter marks what would change) and the
  proposal on the right in an editable buffer named `✻ <file>`. Review, edit
  the right buffer if you like, then decide:

  | Action | Command |
  |---|---|
  | accept with the buffer's current contents | `:claude-diff-accept` (`:cda`), or `:w` in the proposal buffer |
  | reject | `:claude-diff-reject` (`:cdr`), or close the proposal buffer with `:bc` / `:bc!` |
  | keep it pending | `:q` only closes a window, the proposal stays open |

In both modes Helix never writes the file: after an accepted proposal the CLI
writes it, and Helix reloads the buffer if it is open and unmodified. The
CLI's own terminal prompt stays active as well; whichever side answers first
decides, and the CLI closes the proposal in Helix.

The CLI also asks for permission in its own terminal in `acceptEdits`, `plan`
and `bypassPermissions` modes without showing a proposal in the editor.

## Selection and mentions

While a CLI is connected, Helix reports the primary selection of the focused
document (debounced, empty for a bare cursor); the CLI shows it as
"N lines selected". `:claude-mention` inserts `@<path>` — with `#Lstart-end`
when there is a selection — into the CLI prompt. Set `notify-selection = false`
to disable the selection reports.

## Configuration reference

```toml
[editor.claude-ide]
enable = false             # start the server with Helix
name = ""                  # /ide entry name; empty = workspace directory name
notify-selection = true    # send selection_changed to the CLI
diff-mode = "prompt"       # "prompt" | "split"
```

## Limitations

- One CLI at a time: a new connection replaces the previous one (the
  displaced CLI keeps trying to reconnect, so run one `claude` per
  workspace).
- The CLI reconnects on its own if Helix restarts the server on the same
  port; otherwise use `/ide` again.
- Diagnostics come from the language servers running in Helix, so a file
  needs a language server for its diagnostics to reach the CLI, and the
  post-edit report only reflects what the server has published by then.
- `claude -p` (print mode) does not connect to editors.
