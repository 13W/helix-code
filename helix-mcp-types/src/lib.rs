//! `helix-mcp-types` — pure data types and command protocol for the embedded MCP server.
//!
//! Contains all type definitions used for editor ↔ MCP communication.
//! Has no dependency on rmcp, axum, schemars, or the grep crates, so it
//! can be used by any crate (e.g. helix-term) without pulling in the full
//! MCP server stack.

use std::{path::PathBuf, sync::{Arc, Mutex}};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Write / edit types
// ---------------------------------------------------------------------------

/// A line-based text edit (1-indexed, inclusive on both ends).
pub struct TextEdit {
    /// First line to replace (1-indexed, inclusive).
    pub start_line: usize,
    /// Last line to replace (1-indexed, inclusive).
    /// When `end_line < start_line` the edit is a pure insertion (no lines removed).
    pub end_line: usize,
    /// Replacement text.  Use `""` to delete lines.
    pub new_text: String,
}

/// Result returned by write operations.
pub struct WriteResult {
    pub path: PathBuf,
    pub lines_changed: usize,
    pub saved: bool,
}

// ---------------------------------------------------------------------------
// Buffer / file types
// ---------------------------------------------------------------------------

/// Metadata about an open editor buffer.
pub struct BufferInfo {
    pub path: PathBuf,
    pub language: Option<String>,
    pub is_modified: bool,
    pub line_count: usize,
    pub lsp_servers: Vec<String>,
}

/// A 0-indexed, inclusive line range.
pub struct LineRange {
    pub start_line: usize,
    pub end_line: usize,
}

// ---------------------------------------------------------------------------
// Symbol types
// ---------------------------------------------------------------------------

/// A symbol entry returned by `get_symbols_overview`.
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub range: LineRange,
    pub children: Vec<SymbolInfo>,
}

/// A symbol match returned by `find_symbol`.
pub struct SymbolMatch {
    pub name: String,
    pub kind: String,
    pub path: PathBuf,
    pub range: LineRange,
    /// Populated when `include_body = true`.
    pub body: Option<String>,
}

/// A reference location returned by `find_refs`.
pub struct RefLocation {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub preview: String,
}

// ---------------------------------------------------------------------------
// Editor state types
// ---------------------------------------------------------------------------

/// Editor mode returned by `get_cursor`.
pub enum EditorMode {
    Normal,
    Insert,
    Select,
}

/// Cursor state returned by `get_cursor`.
pub struct CursorState {
    pub path: Option<PathBuf>,
    /// 1-indexed line number.
    pub line: usize,
    /// 1-indexed column number.
    pub col: usize,
    pub mode: EditorMode,
    /// Number of cursors (multi-cursor count).
    pub selection_count: usize,
}

/// A single selection range returned by `get_selections`.
pub struct SelectionRange {
    pub anchor_line: usize,
    pub anchor_col: usize,
    pub head_line: usize,
    pub head_col: usize,
    pub is_primary: bool,
    pub text: String,
}

/// Viewport information returned by `get_viewport`.
pub struct ViewportInfo {
    /// 1-indexed first visible line.
    pub first_visible_line: usize,
    pub last_visible_line: usize,
    pub height_lines: usize,
    pub horizontal_offset: usize,
}

// ---------------------------------------------------------------------------
// LSP extra types
// ---------------------------------------------------------------------------

/// A diagnostic item returned by `get_diagnostics`.
pub struct DiagnosticItem {
    pub path: PathBuf,
    /// 0-indexed line number.
    pub line: usize,
    /// 0-indexed column number.
    pub col: usize,
    pub severity: String,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// A code action item returned by `code_actions`.
pub struct CodeActionItem {
    pub title: String,
    pub kind: Option<String>,
}

/// An inlay hint item returned by `inlay_hints`.
pub struct InlayHintItem {
    /// 0-indexed line number.
    pub line: usize,
    /// 0-indexed column number.
    pub col: usize,
    pub label: String,
    /// "type" | "parameter" | "other"
    pub kind: String,
}

/// A completion item returned by `completions`.
pub struct McpCompletionItem {
    pub label: String,
    pub kind: Option<String>,
    pub detail: Option<String>,
    pub insert_text: Option<String>,
}

/// A parameter entry in a signature returned by `signature_help`.
pub struct ParameterInfo {
    pub label: String,
    pub documentation: Option<String>,
}

/// A single function signature returned by `signature_help`.
pub struct SignatureInfo {
    pub label: String,
    pub documentation: Option<String>,
    pub parameters: Vec<ParameterInfo>,
}

/// Result returned by `signature_help`.
pub struct SignatureHelpInfo {
    pub signatures: Vec<SignatureInfo>,
    pub active_signature: Option<u32>,
    pub active_parameter: Option<u32>,
}

// ---------------------------------------------------------------------------
// DAP types
// ---------------------------------------------------------------------------

/// Breakpoint info returned by `get_breakpoints` / `set_breakpoint`.
pub struct BreakpointInfo {
    pub path: PathBuf,
    /// 0-indexed line number.
    pub line: usize,
    pub column: Option<usize>,
    pub condition: Option<String>,
    pub verified: bool,
    pub id: Option<usize>,
    pub message: Option<String>,
}

/// DAP session status returned by `get_dap_status`.
pub struct DapStatus {
    pub active: bool,
    pub paused: bool,
    pub thread_id: Option<usize>,
    pub active_frame: Option<usize>,
}

/// A stack frame entry returned by `get_stack_trace`.
/// Named `StackFrameInfo` to avoid conflict with `helix_dap::StackFrame`.
pub struct StackFrameInfo {
    pub id: usize,
    pub name: String,
    pub path: Option<PathBuf>,
    /// 0-indexed line number.
    pub line: usize,
    pub col: usize,
    pub is_active: bool,
}

/// A scope entry returned by `get_scopes`.
pub struct ScopeInfo {
    pub name: String,
    pub variables_ref: usize,
}

/// A variable entry returned by `get_variables`.
pub struct VariableInfo {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    pub variables_ref: usize,
}

/// A debug template entry returned by `dap_list_templates`.
pub struct DapTemplateInfo {
    pub name: String,
    /// Either `"launch"` or `"attach"`.
    pub request: String,
    /// Positional parameters the template expects.
    pub params: Vec<DapParamInfo>,
}

/// One positional parameter slot in a `DapTemplateInfo`.
pub struct DapParamInfo {
    pub name: String,
    /// Completion hint: `"filename"`, `"directory"`, or `None`.
    pub completion: Option<String>,
    pub default: Option<String>,
}

// ---------------------------------------------------------------------------
// VCS types
// ---------------------------------------------------------------------------

/// The kind of a diff hunk.
pub enum HunkKind {
    Added,
    Deleted,
    Modified,
}

/// A single diff hunk returned by `diff_hunks`.
pub struct DiffHunk {
    pub kind: HunkKind,
    pub before_start: usize,
    pub before_end: usize,
    pub after_start: usize,
    pub after_end: usize,
}

/// Result returned by `diff_hunks`.
pub struct DiffResult {
    pub path: PathBuf,
    pub hunks: Vec<DiffHunk>,
    /// Branch name or commit hash, if available.
    pub head_ref: Option<String>,
}

// ---------------------------------------------------------------------------
// Navigation types
// ---------------------------------------------------------------------------

/// A jumplist entry returned by `get_jumplist`.
pub struct JumpEntry {
    pub path: PathBuf,
    /// 1-indexed line number.
    pub line: usize,
    pub col: usize,
}

// ---------------------------------------------------------------------------
// McpCommand — editor ↔ MCP command protocol
// ---------------------------------------------------------------------------

/// Commands sent from MCP tools to the editor's event loop.
pub enum McpCommand {
    ReadFile {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
    ReadRange {
        path: PathBuf,
        start_line: usize,
        end_line: usize,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
    GetOpenBuffers {
        reply: oneshot::Sender<Vec<BufferInfo>>,
    },
    /// Write (overwrite) a file with new content.
    WriteFile {
        path: PathBuf,
        content: String,
        reply: oneshot::Sender<anyhow::Result<WriteResult>>,
    },
    /// Apply a set of line-based edits atomically.
    ApplyEdits {
        path: PathBuf,
        edits: Vec<TextEdit>,
        reply: oneshot::Sender<anyhow::Result<WriteResult>>,
    },
    /// Insert text before `line` (1-indexed).
    InsertText {
        path: PathBuf,
        /// Line number before which to insert (1-indexed).
        line: usize,
        text: String,
        reply: oneshot::Sender<anyhow::Result<WriteResult>>,
    },
    /// Rename the symbol at the given position via LSP.
    RenameSymbol {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        /// 0-indexed column number.
        col: usize,
        new_name: String,
        reply: oneshot::Sender<anyhow::Result<WriteResult>>,
    },
    /// Replace the body of a symbol identified by name-path (e.g. `"MyStruct/my_method"`).
    ReplaceSymbol {
        path: PathBuf,
        name_path: String,
        body: String,
        reply: oneshot::Sender<anyhow::Result<WriteResult>>,
    },
    /// Show a diff and ask the user for y/n permission.
    /// Sent internally by write command handlers; not exposed as an MCP tool.
    RequestPermission {
        tool_name: String,
        diff: String,
        reply: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
    },
    /// Get the symbol hierarchy for a file via LSP `textDocument/documentSymbol`.
    GetSymbolsOverview {
        path: PathBuf,
        /// 0 = top-level only, 1 = top-level + immediate children.
        depth: u8,
        reply: oneshot::Sender<anyhow::Result<(Vec<SymbolInfo>, String)>>,
    },
    /// Search symbols workspace-wide via LSP `workspace/symbol`.
    FindSymbol {
        query: String,
        /// Optional path prefix to filter results.
        path: Option<PathBuf>,
        include_body: bool,
        reply: oneshot::Sender<anyhow::Result<Vec<SymbolMatch>>>,
    },
    /// Find all references to the symbol at (line, col) via LSP `textDocument/references`.
    FindRefs {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        /// 0-indexed column number.
        col: usize,
        reply: oneshot::Sender<anyhow::Result<Vec<RefLocation>>>,
    },
    /// Read the body of a symbol identified by name-path (e.g. `"MyStruct/my_method"`).
    ReadSymbol {
        path: PathBuf,
        name_path: String,
        reply: oneshot::Sender<anyhow::Result<SymbolMatch>>,
    },
    /// Get the current cursor position and editor mode.
    GetCursor {
        reply: oneshot::Sender<CursorState>,
    },
    /// Get all selection ranges for the document at `path`.
    GetSelections {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<Vec<SelectionRange>>>,
    },
    /// Get the visible viewport range for the document at `path`.
    GetViewport {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<ViewportInfo>>,
    },
    /// Get diagnostics. `path = None` returns all workspace diagnostics.
    GetDiagnostics {
        path: Option<PathBuf>,
        reply: oneshot::Sender<Vec<DiagnosticItem>>,
    },
    /// Get hover documentation for the symbol at (line, col).
    Hover {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        /// 0-indexed column number.
        col: usize,
        reply: oneshot::Sender<anyhow::Result<Option<String>>>,
    },
    /// Get available code actions at (line, col).
    CodeActions {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        /// 0-indexed column number.
        col: usize,
        reply: oneshot::Sender<anyhow::Result<Vec<CodeActionItem>>>,
    },
    /// Get inlay hints for the given line range (0-indexed).
    InlayHints {
        path: PathBuf,
        start_line: usize,
        end_line: usize,
        reply: oneshot::Sender<anyhow::Result<Vec<InlayHintItem>>>,
    },
    /// Get completion items at (line, col).
    Completions {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        /// 0-indexed column number.
        col: usize,
        reply: oneshot::Sender<anyhow::Result<Vec<McpCompletionItem>>>,
    },
    /// Get function signature help (active signature + parameters) at (line, col).
    SignatureHelp {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        /// 0-indexed column number.
        col: usize,
        reply: oneshot::Sender<anyhow::Result<Option<SignatureHelpInfo>>>,
    },

    // --- DAP: Breakpoints ---

    /// Return all breakpoints, optionally filtered to a single file path.
    GetBreakpoints {
        path: Option<PathBuf>,
        reply: oneshot::Sender<Vec<BreakpointInfo>>,
    },
    /// Set a breakpoint (requires user approval via prompt).
    SetBreakpoint {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        condition: Option<String>,
        reply: Arc<Mutex<Option<oneshot::Sender<anyhow::Result<BreakpointInfo>>>>>,
    },
    /// Remove the breakpoint at the given path and 0-indexed line.
    RemoveBreakpoint {
        path: PathBuf,
        /// 0-indexed line number.
        line: usize,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },

    // --- DAP: State ---

    /// Get current DAP session status (safe to call even when no session is active).
    GetDapStatus {
        reply: oneshot::Sender<DapStatus>,
    },
    /// Get the call stack for the active (or specified) thread.
    GetStackTrace {
        /// `None` uses the active thread.
        thread_id: Option<usize>,
        reply: oneshot::Sender<anyhow::Result<Vec<StackFrameInfo>>>,
    },
    /// Get the variable scopes for the given stack frame id.
    GetScopes {
        frame_id: usize,
        reply: oneshot::Sender<anyhow::Result<Vec<ScopeInfo>>>,
    },
    /// Get variables for a scope identified by its `variables_ref` (from `get_scopes`).
    GetVariables {
        variables_ref: usize,
        reply: oneshot::Sender<anyhow::Result<Vec<VariableInfo>>>,
    },

    // --- DAP: Execution control ---

    /// Resume execution of the paused thread.
    DapContinue {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Pause the running thread.
    DapPause {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Step over (next line).
    DapStepOver {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Step into a function call.
    DapStepIn {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Step out of the current function.
    DapStepOut {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },

    // --- VCS: Diff ---

    /// Get diff hunks for a file (must be open in the editor).
    GetDiffHunks {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<DiffResult>>,
    },
    /// Get the HEAD base content via diff providers.
    GetDiffBase {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },

    // --- DAP: Session lifecycle ---

    /// List debug templates for the focused document's language.
    DapListTemplates {
        reply: oneshot::Sender<anyhow::Result<Vec<DapTemplateInfo>>>,
    },
    /// Launch (or attach to) a debug session.
    DapLaunch {
        /// Template name. `None` = first template.
        template_name: Option<String>,
        /// Positional parameters for the template.
        params: Vec<String>,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Terminate the active debug session.
    DapTerminate {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },

    /// Load a file into the editor buffer without displaying it to the user.
    LoadFile {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
    /// Unload a background-loaded file from the editor buffer.
    /// Fails if the file is currently visible in any view.
    UnloadFile {
        path: PathBuf,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },

    // --- Registers & Jumplist ---

    /// Read values from a named register.
    ReadRegister {
        name: char,
        reply: oneshot::Sender<anyhow::Result<Vec<String>>>,
    },
    /// Write values to a named register (a-z, A-Z, '+', '*' only).
    WriteRegister {
        name: char,
        values: Vec<String>,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Get the jumplist for the current view.
    GetJumplist {
        reply: oneshot::Sender<Vec<JumpEntry>>,
    },
    /// Hybrid find-replace / line-range edit.
    EditFile {
        path: PathBuf,
        old_string: Option<String>,
        new_string: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
        replace_all: bool,
        reply: oneshot::Sender<anyhow::Result<WriteResult>>,
    },
}
