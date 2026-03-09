//! Embedded MCP (Model Context Protocol) server for Helix.
//!
//! Starts an HTTP server that speaks the MCP Streamable-HTTP protocol,
//! allowing AI agents (e.g. Claude Code via ACP) to connect and use
//! Helix editor tools without manual `mcp.json` configuration.

use anyhow::Result;
use axum::{Router, extract::{Request, State}, response::IntoResponse, routing::any};
use rmcp::{
    ServerHandler,
    handler::server::tool::ToolRouter,
    model::{
        CallToolResult, Content, Implementation, InitializeResult,
        ProtocolVersion, ServerCapabilities,
    },
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager,
    tower::{StreamableHttpService, StreamableHttpServerConfig},
};
use std::{net::SocketAddr, sync::Arc};

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
