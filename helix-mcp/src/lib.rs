//! Embedded MCP (Model Context Protocol) server for Helix.
//!
//! Starts an HTTP server that speaks the MCP Streamable-HTTP protocol,
//! allowing AI agents (e.g. Claude Code via ACP) to connect and use
//! Helix editor tools without manual `mcp.json` configuration.

mod tools;
pub mod channel;
pub(crate) mod server;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::OnceLock;

// Re-export all pure types and the McpCommand enum from the companion types crate.
pub use helix_mcp_types::*;

// Re-export the editor channel + helpers as the crate's public surface.
pub use channel::{
    MAX_INLINE_BYTES, MCP_AUTO_APPROVE,
    truncate_to_char_boundary,
    fetch_file_content, fetch_diff_base_content,
    init_editor_channel, editor_tx,
    set_auto_approve, auto_approve,
};

// ---------------------------------------------------------------------------

/// Cached address of the running MCP server (singleton).
static MCP_SERVER_ADDR: OnceLock<SocketAddr> = OnceLock::new();

/// Start the MCP Streamable-HTTP server.
///
/// Binds to `addr` (or `127.0.0.1:0` for a random port) and spawns the
/// server as a background tokio task.  Returns the actual bound address
/// (including the OS-assigned port when `addr` is `None`).
pub async fn run_mcp_server(addr: Option<SocketAddr>) -> Result<SocketAddr> {
    let app = server::build_mcp_app();
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
