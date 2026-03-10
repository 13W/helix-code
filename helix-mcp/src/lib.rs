//! Embedded MCP (Model Context Protocol) server for Helix.
//!
//! Starts an HTTP server that speaks the MCP Streamable-HTTP protocol,
//! allowing AI agents (e.g. Claude Code via ACP) to connect and use
//! Helix editor tools without manual `mcp.json` configuration.

mod tools;

use anyhow::Result;
use axum::{Router, extract::{Request, State}, response::IntoResponse, routing::any};
use rmcp::{
    ServerHandler,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeResult,
        ProtocolVersion, ServerCapabilities,
    },
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager,
    tower::{StreamableHttpService, StreamableHttpServerConfig},
};
use std::{net::SocketAddr, path::PathBuf, sync::{Arc, Mutex, OnceLock}};
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// Editor ↔ MCP command types
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

/// A jumplist entry returned by `get_jumplist`.
pub struct JumpEntry {
    pub path: PathBuf,
    /// 1-indexed line number.
    pub line: usize,
    pub col: usize,
}

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
    /// Get variables for the active (or specified) stack frame.
    GetVariables {
        /// `None` uses the active frame.
        frame_id: Option<usize>,
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
}

// ---------------------------------------------------------------------------
// Global editor channel (one sender, one receiver; single Application instance)
// ---------------------------------------------------------------------------

static MCP_EDITOR_TX: OnceLock<mpsc::Sender<McpCommand>> = OnceLock::new();

/// Cached address of the running MCP server (singleton).
static MCP_SERVER_ADDR: OnceLock<SocketAddr> = OnceLock::new();

/// Called once by `Application::new()` to wire up the editor ↔ MCP channel.
/// Returns the `Receiver` end for Application to poll in the event loop.
pub fn init_editor_channel() -> mpsc::Receiver<McpCommand> {
    let (tx, rx) = mpsc::channel(64);
    // OnceLock::set is a no-op on subsequent calls (integration test restarts etc.)
    let _ = MCP_EDITOR_TX.set(tx);
    rx
}

/// Returns a clone of the Sender for use by MCP tools.
pub fn editor_tx() -> Option<mpsc::Sender<McpCommand>> {
    MCP_EDITOR_TX.get().cloned()
}

/// When `true`, all MCP write operations are applied immediately without prompting the user.
static MCP_AUTO_APPROVE: std::sync::atomic::AtomicBool =
    // std::sync::atomic::AtomicBool::new(false);
    // TODO: should be fixed with permissions
    std::sync::atomic::AtomicBool::new(true);

/// Enable or disable automatic approval of MCP write operations.
pub fn set_auto_approve(val: bool) {
    MCP_AUTO_APPROVE.store(val, std::sync::atomic::Ordering::Relaxed);
}

/// Returns `true` if MCP write operations should be applied without user confirmation.
pub fn auto_approve() -> bool {
    MCP_AUTO_APPROVE.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// MCP server handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct HelixMcpServer {
    tool_router: ToolRouter<Self>,
}

#[rmcp::tool_router]
impl HelixMcpServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[rmcp::tool(description = "Health-check — returns pong")]
    async fn ping(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }

    #[rmcp::tool(description = "List directory contents. Returns entries with path, kind (file/dir), and size for files.")]
    async fn list_dir(
        &self,
        params: Parameters<tools::fs::ListDirParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = tools::fs::handle_list_dir(params.0);
        let json = serde_json::to_string_pretty(&result).map_err(tools::fs::to_mcp_err)?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[rmcp::tool(description = "Find files matching a glob pattern (respects .gitignore). Returns list of matching file paths.")]
    async fn find_files(
        &self,
        params: Parameters<tools::fs::FindFilesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = tools::fs::handle_find_files(params.0).map_err(tools::fs::to_mcp_err)?;
        let json = serde_json::to_string_pretty(&result).map_err(tools::fs::to_mcp_err)?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[rmcp::tool(description = "Search file contents with a regex pattern. Returns matches with line numbers and optional context lines.")]
    async fn search(
        &self,
        params: Parameters<tools::fs::SearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = tools::fs::handle_search(params.0).map_err(tools::fs::to_mcp_err)?;
        let json = serde_json::to_string_pretty(&result).map_err(tools::fs::to_mcp_err)?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[rmcp::tool(description = "Read a file — from editor buffer if open (sees unsaved changes), otherwise from disk")]
    async fn read_file(
        &self,
        params: Parameters<tools::read::ReadFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::read::handle_read_file(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Read a line range from a file (0-indexed, end_line inclusive). Includes line numbers in output.")]
    async fn read_range(
        &self,
        params: Parameters<tools::read::ReadRangeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::read::handle_read_range(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "List all open editor buffers with path, language, modified status, line count, and LSP servers")]
    async fn get_open_buffers(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::read::handle_get_open_buffers().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Write (overwrite) a file with new content. Shows a diff and requires user approval before applying.")]
    async fn write_file(
        &self,
        params: Parameters<tools::write::WriteFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_write_file(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Apply multiple line-based text edits to a file atomically. Shows a diff and requires user approval.")]
    async fn edit_file(
        &self,
        params: Parameters<tools::write::EditFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_edit_file(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Insert text before a given line (1-indexed). Requires user approval.")]
    async fn insert_text(
        &self,
        params: Parameters<tools::write::InsertTextParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_insert_text(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Rename a symbol at the given file position via LSP. Applies workspace-wide edits after user approval.")]
    async fn rename_symbol(
        &self,
        params: Parameters<tools::write::RenameSymbolParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_rename_symbol(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Replace the body of a symbol identified by name-path (e.g. 'MyStruct' or 'MyStruct/my_method'). Shows a diff and requires user approval.")]
    async fn replace_symbol(
        &self,
        params: Parameters<tools::write::ReplaceSymbolParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_replace_symbol(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get a high-level overview of symbols (functions, structs, etc.) in a file via LSP documentSymbol.")]
    async fn get_symbols_overview(
        &self,
        params: Parameters<tools::symbols::GetSymbolsOverviewParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::symbols::handle_get_symbols_overview(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Find symbols by name across the workspace via LSP workspace/symbol.")]
    async fn find_symbol(
        &self,
        params: Parameters<tools::symbols::FindSymbolParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::symbols::handle_find_symbol(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Find all references to the symbol at the given file position via LSP textDocument/references.")]
    async fn find_refs(
        &self,
        params: Parameters<tools::symbols::FindRefsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::symbols::handle_find_refs(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Read the source body of a symbol by name-path (e.g. 'MyStruct' or 'MyStruct/my_method').")]
    async fn read_symbol(
        &self,
        params: Parameters<tools::symbols::ReadSymbolParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::symbols::handle_read_symbol(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get the current cursor position, editor mode, and selection count.")]
    async fn get_cursor(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::editor::handle_get_cursor().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get all selection ranges for a file, including anchor/head positions and selected text.")]
    async fn get_selections(
        &self,
        params: Parameters<tools::editor::GetSelectionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::editor::handle_get_selections(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get the visible viewport range (first/last visible line) for a file.")]
    async fn get_viewport(
        &self,
        params: Parameters<tools::editor::GetViewportParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::editor::handle_get_viewport(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get diagnostic information for a specific file from the language server.")]
    async fn get_diagnostics(
        &self,
        params: Parameters<tools::lsp_extras::GetDiagnosticsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_get_diagnostics(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get hover information (type, documentation) for a symbol at the specified position.")]
    async fn hover(
        &self,
        params: Parameters<tools::lsp_extras::HoverParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_hover(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get available code actions (quick fixes, refactors) at the specified position.")]
    async fn code_actions(
        &self,
        params: Parameters<tools::lsp_extras::CodeActionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_code_actions(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get inlay hints (type annotations, parameter names) for a line range in a file.")]
    async fn inlay_hints(
        &self,
        params: Parameters<tools::lsp_extras::InlayHintsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_inlay_hints(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get completion suggestions at the specified position in a file.")]
    async fn completions(
        &self,
        params: Parameters<tools::lsp_extras::CompletionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_completions(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get function signature help (active signature and parameter info) at the specified position.")]
    async fn signature_help(
        &self,
        params: Parameters<tools::lsp_extras::SignatureHelpParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_signature_help(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Return all breakpoints, optionally filtered by file path.")]
    async fn get_breakpoints(
        &self,
        params: Parameters<tools::dap::GetBreakpointsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_get_breakpoints(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Set a breakpoint at the given path and line (0-indexed). Requires user approval.")]
    async fn set_breakpoint(
        &self,
        params: Parameters<tools::dap::SetBreakpointParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_set_breakpoint(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Remove the breakpoint at the given path and line (0-indexed).")]
    async fn remove_breakpoint(
        &self,
        params: Parameters<tools::dap::RemoveBreakpointParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_remove_breakpoint(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get current DAP debugger session status (active, paused, thread/frame info).")]
    async fn get_dap_status(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_get_dap_status().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get the call stack frames for the active (or specified) thread. Requires debugger to be paused.")]
    async fn get_stack_trace(
        &self,
        params: Parameters<tools::dap::GetStackTraceParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_get_stack_trace(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get variable scopes for the given stack frame id. Requires debugger to be paused.")]
    async fn get_scopes(
        &self,
        params: Parameters<tools::dap::GetScopesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_get_scopes(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get variables for the active (or specified) stack frame. Requires debugger to be paused.")]
    async fn get_variables(
        &self,
        params: Parameters<tools::dap::GetVariablesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_get_variables(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Resume execution of the paused debugger thread.")]
    async fn dap_continue(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_continue().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Pause the running debugger thread.")]
    async fn dap_pause(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_pause().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Step over the current line (next line, does not step into calls).")]
    async fn dap_step_over(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_step_over().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Step into a function call on the current line.")]
    async fn dap_step_in(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_step_in().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Step out of the current function back to its caller.")]
    async fn dap_step_out(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_step_out().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get VCS diff hunks for a file (requires it to be open in the editor).")]
    async fn diff_hunks(
        &self,
        params: Parameters<tools::vcs::DiffHunksParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::vcs::handle_diff_hunks(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get the HEAD (base) content of a file via VCS diff providers.")]
    async fn diff_base(
        &self,
        params: Parameters<tools::vcs::DiffBaseParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::vcs::handle_diff_base(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Read the values stored in a named Helix register (e.g. '/', '+', 'a').")]
    async fn read_register(
        &self,
        params: Parameters<tools::registers::ReadRegisterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::registers::handle_read_register(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Write values to a named Helix register. Only alphabetic registers and '+' / '*' are writable.")]
    async fn write_register(
        &self,
        params: Parameters<tools::registers::WriteRegisterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::registers::handle_write_register(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get the jumplist for the current view (navigation history).")]
    async fn get_jumplist(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::registers::handle_get_jumplist().await
            .map_err(tools::fs::to_mcp_err)
    }
}

#[rmcp::tool_handler]
impl ServerHandler for HelixMcpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "helix".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "You are connected to the Helix editor MCP server. \
                 This server has EXCLUSIVE ownership of all file read, write, and edit operations. \
                 NEVER use built-in Read, Write, Edit, Glob, or Grep tools — \
                 always use the MCP tools provided by this server instead: \
                 read_file, read_range, write_file, edit_file, insert_text, \
                 find_files, search, list_dir. \
                 read_file reads from the live editor buffer and sees unsaved changes; \
                 write_file and edit_file show a diff and require user approval before applying."
                    .into(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP handler shim
// ---------------------------------------------------------------------------

type McpService = StreamableHttpService<HelixMcpServer, LocalSessionManager>;

async fn mcp_handler(
    State(svc): State<Arc<McpService>>,
    req: Request,
) -> impl IntoResponse {
    svc.handle(req).await
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the MCP Streamable-HTTP server.
///
/// Binds to `addr` (or `127.0.0.1:0` for a random port) and spawns the
/// server as a background tokio task.  Returns the actual bound address
/// (including the OS-assigned port when `addr` is `None`).
pub async fn run_mcp_server(addr: Option<SocketAddr>) -> Result<SocketAddr> {
    let mcp_service: McpService = StreamableHttpService::new(
        || Ok(HelixMcpServer::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = Router::new()
        .route("/mcp", any(mcp_handler))
        .with_state(Arc::new(mcp_service));

    let bind_addr: SocketAddr = addr.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("helix-mcp: server error: {e}");
        }
    });

    log::info!("helix-mcp: MCP server listening at http://{local_addr}/mcp");
    let _ = MCP_SERVER_ADDR.set(local_addr);
    Ok(local_addr)
}

/// Returns the address of the already-running MCP server, or starts one if none is running.
/// Safe to call from multiple tasks — only one server is ever started.
pub async fn get_or_start_mcp_server() -> Result<SocketAddr> {
    if let Some(&addr) = MCP_SERVER_ADDR.get() {
        return Ok(addr);
    }
    let addr = run_mcp_server(None).await?;
    Ok(*MCP_SERVER_ADDR.get().unwrap_or(&addr))
}
