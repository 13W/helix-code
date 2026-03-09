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
}

// ---------------------------------------------------------------------------
// Global editor channel (one sender, one receiver; single Application instance)
// ---------------------------------------------------------------------------

static MCP_EDITOR_TX: OnceLock<mpsc::Sender<McpCommand>> = OnceLock::new();

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
            instructions: None,
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
    Ok(local_addr)
}
