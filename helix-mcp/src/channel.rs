//! Global editor channel, auto-approve flag, and file-content helpers.
//!
//! Accessed by both the MCP server tools and the `lib.rs` entry point.

use std::{path::PathBuf, sync::OnceLock};
use tokio::sync::{mpsc, oneshot};
use helix_mcp_types::McpCommand;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum bytes to return inline in a tool response.
/// Files larger than this get their content truncated to this size,
/// plus a `resource_link` pointing to the full content via `read_resource`.
pub const MAX_INLINE_BYTES: usize = 128 * 1024;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a UTF-8 string to at most `max_bytes`, aligned to a char boundary.
pub fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

/// Read file content from the editor buffer (if open) or from disk.
/// Returns `(content, from_buffer)`.
pub async fn fetch_file_content(path: PathBuf) -> anyhow::Result<(String, bool)> {
    if let Some(tx) = editor_tx() {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(McpCommand::ReadFile { path, reply: reply_tx }).await?;
        let content = reply_rx.await??;
        Ok((content, true))
    } else {
        let content = std::fs::read_to_string(&path)?;
        Ok((content, false))
    }
}

/// Read the HEAD (VCS base) content of a file via diff providers.
pub async fn fetch_diff_base_content(path: PathBuf) -> anyhow::Result<String> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::GetDiffBase { path, reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("reply channel closed"))?
}

// ---------------------------------------------------------------------------
// Global editor channel
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
// Auto-approve flag
// ---------------------------------------------------------------------------

/// When `true`, all MCP write operations are applied immediately without prompting the user.
pub static MCP_AUTO_APPROVE: std::sync::atomic::AtomicBool =
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
