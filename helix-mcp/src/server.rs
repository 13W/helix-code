//! `HelixMcpServer` — MCP tool router, `ServerHandler` impl, and HTTP shim.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use axum::{Router, extract::{Request, State}, response::IntoResponse, routing::any};
use rmcp::{
    ErrorData as McpError,
    ServerHandler,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeResult,
        AnnotateAble, ListResourceTemplatesResult, PaginatedRequestParams, ProtocolVersion,
        RawResourceTemplate, ReadResourceRequestParams,
        ReadResourceResult, ResourceContents, ServerCapabilities,
    },
    service::{RequestContext, RoleServer},
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager,
    tower::{StreamableHttpService, StreamableHttpServerConfig},
};

use crate::{channel, tools};

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct HelixMcpServer {
    tool_router: ToolRouter<Self>,
}

#[rmcp::tool_router]
impl HelixMcpServer {
    pub(crate) fn new() -> Self {
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

    #[rmcp::tool(description = "Search file contents with a regex pattern. Returns matches with line numbers and optional context lines. Max 500 matches.")]
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
    async fn patch_file(
        &self,
        params: Parameters<tools::write::PatchFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_patch_file(params.0).await
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

    #[rmcp::tool(description = "Find-and-replace an exact string in a file, replace a line range, or both. Shows diff and requires user approval. Params: old_string (exact text to find), new_string (replacement, required), start_line/end_line (1-indexed scope or range).")]
    async fn edit_file(
        &self,
        params: Parameters<tools::write::EditFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::write::handle_edit_file(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Rename a symbol at the given file position (0-indexed line/col) via LSP. Applies workspace-wide edits after user approval.")]
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

    #[rmcp::tool(description = "Find all references to the symbol at the given file position (0-indexed line/col) via LSP textDocument/references.")]
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

    #[rmcp::tool(description = "Get the current cursor position (1-indexed line/col), editor mode, and selection count.")]
    async fn get_cursor(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::editor::handle_get_cursor().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get all selection ranges for a file, including anchor/head positions (1-indexed) and selected text.")]
    async fn get_selections(
        &self,
        params: Parameters<tools::editor::GetSelectionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::editor::handle_get_selections(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get the visible viewport range (1-indexed first/last visible line) for a file.")]
    async fn get_viewport(
        &self,
        params: Parameters<tools::editor::GetViewportParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::editor::handle_get_viewport(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get diagnostics (errors/warnings) from the language server. Returns 0-indexed line/col. Omit path to get all workspace diagnostics.")]
    async fn get_diagnostics(
        &self,
        params: Parameters<tools::lsp_extras::GetDiagnosticsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_get_diagnostics(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get hover information (type, documentation) for a symbol at the specified position (0-indexed line/col). Returns null (not error) when no hover info available.")]
    async fn hover(
        &self,
        params: Parameters<tools::lsp_extras::HoverParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_hover(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get available code actions (quick fixes, refactors) at the specified position (0-indexed line/col).")]
    async fn code_actions(
        &self,
        params: Parameters<tools::lsp_extras::CodeActionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_code_actions(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get inlay hints (type annotations, parameter names) for a line range (0-indexed) in a file.")]
    async fn inlay_hints(
        &self,
        params: Parameters<tools::lsp_extras::InlayHintsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_inlay_hints(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get completion suggestions at the specified position (0-indexed line/col) in a file.")]
    async fn completions(
        &self,
        params: Parameters<tools::lsp_extras::CompletionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::lsp_extras::handle_completions(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get function signature help (active signature and parameter info) at the specified position (0-indexed line/col).")]
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

    #[rmcp::tool(description = "Get variables for a scope. Pass variables_ref from get_scopes to query a specific scope directly. Or pass frame_id to auto-resolve locals without needing get_scopes first (scope_name defaults to 'local'; pass 'register' to get CPU registers). Requires debugger to be paused.")]
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

    #[rmcp::tool(description = "List debug templates available for the focused document's language. Call this before dap_launch to discover template names and required parameters.")]
    async fn dap_list_templates(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_list_templates().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Launch a debug session for the focused document. Use dap_list_templates first to discover the template_name and required params.")]
    async fn dap_launch(
        &self,
        params: Parameters<tools::dap::DapLaunchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_launch(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Terminate the active debug session.")]
    async fn dap_terminate(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::dap::handle_dap_terminate().await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Get VCS diff hunks for a file (requires it to be open in the editor). Returns hunks with kind: added/deleted/modified.")]
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

    #[rmcp::tool(description = "Load a file into the editor buffer without displaying it. Use before calling diff_hunks on files not currently open. The file will not appear in any view.")]
    async fn load_file(
        &self,
        params: Parameters<tools::buffer::LoadFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::buffer::handle_load_file(params.0).await
            .map_err(tools::fs::to_mcp_err)
    }

    #[rmcp::tool(description = "Unload a background-loaded file from the editor buffer. Only works on files not currently visible in any view. Use after you are done with diff_hunks or other buffer-dependent tools.")]
    async fn unload_file(
        &self,
        params: Parameters<tools::buffer::UnloadFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::buffer::handle_unload_file(params.0).await
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

    #[rmcp::tool(description = "Get the jumplist for the current view (navigation history, up to 30 entries).")]
    async fn get_jumplist(
        &self,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::registers::handle_get_jumplist().await
            .map_err(tools::fs::to_mcp_err)
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[rmcp::tool_handler]
impl ServerHandler for HelixMcpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().enable_resources().build(),
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
                 read_file, read_range, write_file, edit_file, patch_file, insert_text, \
                 find_files, search, list_dir. \
                 read_file reads from the live editor buffer and sees unsaved changes; \
                 write_file, edit_file and patch_file show a diff and require user approval before applying."
                    .into(),
            ),
        }
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                RawResourceTemplate {
                    uri_template: "helix://buffer/{path}".into(),
                    name: "Editor buffer / file (full content)".into(),
                    title: None,
                    description: Some(
                        "Full file content from the editor buffer (includes unsaved changes). \
                         Use read_resource when read_file truncates a large file."
                            .into(),
                    ),
                    mime_type: Some("text/plain".into()),
                    icons: None,
                }
                .no_annotation(),
                RawResourceTemplate {
                    uri_template: "helix://diff-base/{path}".into(),
                    name: "VCS HEAD base content (full)".into(),
                    title: None,
                    description: Some(
                        "Full HEAD version of a file from VCS. \
                         Use read_resource when diff_base truncates a large file."
                            .into(),
                    ),
                    mime_type: Some("text/plain".into()),
                    icons: None,
                }
                .no_annotation(),
            ],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = &request.uri;
        let content = if let Some(p) = uri.strip_prefix("helix://buffer") {
            channel::fetch_file_content(PathBuf::from(p))
                .await
                .map(|(c, _)| c)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else if let Some(p) = uri.strip_prefix("helix://diff-base") {
            channel::fetch_diff_base_content(PathBuf::from(p))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            return Err(McpError::internal_error(
                format!("Unknown resource URI: {uri}"),
                None,
            ));
        };
        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(content, uri.clone())],
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP handler shim
// ---------------------------------------------------------------------------

pub(crate) type McpService = StreamableHttpService<HelixMcpServer, LocalSessionManager>;

pub(crate) async fn mcp_handler(
    State(svc): State<Arc<McpService>>,
    req: Request,
) -> impl IntoResponse {
    svc.handle(req).await
}

// ---------------------------------------------------------------------------
// Server factory
// ---------------------------------------------------------------------------

pub(crate) fn build_mcp_app() -> Router {
    let mcp_service: McpService = StreamableHttpService::new(
        || Ok(HelixMcpServer::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route("/mcp", any(mcp_handler))
        .with_state(Arc::new(mcp_service))
}
