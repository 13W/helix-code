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
port, the lock file and one line per connected CLI (`#N`, pid, permission
mode, waiting proposals, working directory, focus); `:claude-ide-stop` stops
the server and removes the lock file. The statusline element
`claude-ide-indicator` shows `✻` while the server runs, `✻◆` once a CLI is
connected and `✻◆N` with `N` CLIs connected.

Then, in a terminal whose current directory is inside the workspace:

```sh
claude --ide          # connect at start-up
```

or type `/ide` inside a running `claude` session and pick the Helix entry.
The CLI finds Helix through `~/.claude/ide/<port>.lock` (honouring
`CLAUDE_CONFIG_DIR`); the lock file is removed when Helix exits.

### Several CLIs at once

Up to `max-clients` (default 4) `claude` sessions can use the same Helix, for
example one per task or one per git worktree below the workspace. Each is
identified by its pid (shown in proposal titles and buffer names) and by a
connection number `#N`. Connections beyond the limit are refused with HTTP
503; the CLI retries a few times with a growing delay, then reports the IDE
as unavailable in `/ide`.

| Command | Effect |
|---|---|
| `:claude-ide-status` | table of connected CLIs: `#N`, pid, permission mode, waiting proposals, cwd (Linux only, `?` elsewhere), `●` focus / `○` default target |
| `:claude-ide-focus <pid\|#N>` | make that CLI the target of `:claude-mention`; without an argument a picker opens; `none` clears the focus (it is also cleared when that CLI disconnects) |
| `:claude-mention [pid\|#N]` | mention the current file in that CLI; without an argument: the focused CLI, or the only one; with several CLIs and no focus a picker opens |
| `:claude-ide-disconnect <pid\|#N>` | close that connection (its waiting proposals are rejected). The CLI tries to reconnect a few times on its own; use `/exit` in the CLI to end it for good |

## Reviewing proposals

When the CLI wants to edit or create a file and asks for permission (its
default permission mode), the proposal is shown in Helix according to
`diff-mode`:

- **`prompt`** (default): a dialog with a unified diff preview and
  `Apply` / `Reject`. `Enter` applies, `Escape` rejects.
- **`split`**: a vertical split with the current file on the left (read-only
  while the proposal is open; the gutter marks what would change) and the
  proposal on the right in an editable buffer named `✻ <file> [<pid>]`
  (`[#N]` before the CLI has announced its pid). Both cursors start on the
  first changed line and the status line says how many changes there are;
  the proposal buffer is never reported to the CLI as a file. Review, edit
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

With several CLIs connected, prompts are titled `Claude Code (pid …) proposes
changes to …` and are shown one after another; splits can be open side by side
(one per proposal). Each CLI only ever closes its own proposals — a new turn in
one CLI does not dismiss what another CLI is waiting for. Two CLIs may propose
changes to the same file; Helix does not arbitrate, the CLI whose proposal is
accepted writes the file.

The CLI also asks for permission in its own terminal in `acceptEdits`, `plan`
and `bypassPermissions` modes without showing a proposal in the editor.

## Selection and mentions

While a CLI is connected, Helix reports the primary selection of the focused
document (debounced, empty for a bare cursor); the CLI shows it as
"N lines selected". `:claude-mention [pid|#N]` inserts `@<path>` — with
`#Lstart-end` when there is a selection — into the prompt of one CLI (see
[Several CLIs at once](#several-clis-at-once) for how the target is chosen).
Selection reports go to every connected CLI; set `notify-selection = false`
to disable them.

## Configuration reference

```toml
[editor.claude-ide]
enable = false             # start the server with Helix
name = ""                  # /ide entry name; empty = workspace directory name
notify-selection = true    # send selection_changed to the CLI
diff-mode = "prompt"       # "prompt" | "split"
max-clients = 4            # concurrent CLIs; more are refused (must be >= 1)
```

## Limitations

- At most `max-clients` CLIs; further ones are refused (HTTP 503) rather
  than displacing a connected one.
- The CLI reconnects on its own if Helix restarts the server on the same
  port or after `:claude-ide-disconnect`; otherwise use `/ide` again.
- The CLI's working directory in `:claude-ide-status` is only available on
  Linux (`/proc`); other platforms show `?`.
- Diagnostics come from the language servers running in Helix, so a file
  needs a language server for its diagnostics to reach the CLI, and the
  post-edit report only reflects what the server has published by then.
- `claude -p` (print mode) does not connect to editors.
