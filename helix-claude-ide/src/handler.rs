//! Editor-backed [`ToolHandler`]: forwards tool calls to the Helix event loop
//! over the existing `McpCommand` channel (`helix-mcp-types`).
//!
//! The individual tools are filled in by later tasks; this module owns the
//! shared plumbing (channel, registry of pending diffs, notifier).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use helix_mcp_types::McpCommand;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::tools::{ToolHandler, ToolResult};
use crate::{IdeServerHandle, Notifier};

/// Tool handler wired to the editor.
pub struct EditorHandler {
    mcp_tx: mpsc::Sender<McpCommand>,
    notifier: Mutex<Option<Notifier>>,
}

impl EditorHandler {
    pub fn new(mcp_tx: mpsc::Sender<McpCommand>) -> Self {
        EditorHandler {
            mcp_tx,
            notifier: Mutex::new(None),
        }
    }

    pub fn mcp_tx(&self) -> &mpsc::Sender<McpCommand> {
        &self.mcp_tx
    }

    /// Notifier for the currently connected client, if any.
    pub fn notifier(&self) -> Option<Notifier> {
        self.notifier.lock().unwrap().clone()
    }

    /// Number of `openDiff` calls waiting for a user decision.
    pub fn pending_diff_count(&self) -> usize {
        0
    }
}

#[async_trait]
impl ToolHandler for EditorHandler {
    async fn call(&self, name: &str, _arguments: Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error(format!("{name}: not implemented")))
    }

    fn on_client_connected(&self, notifier: Notifier) {
        *self.notifier.lock().unwrap() = Some(notifier);
    }

    fn on_client_disconnected(&self) {
        *self.notifier.lock().unwrap() = None;
    }
}

/// A running IDE server together with its editor-side handler, as stored on
/// the editor while the integration is active.
#[derive(Clone)]
pub struct Session {
    pub handle: IdeServerHandle,
    pub handler: Arc<EditorHandler>,
}

impl Session {
    pub fn port(&self) -> u16 {
        self.handle.port()
    }

    pub fn is_connected(&self) -> bool {
        self.handle.is_connected()
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("handle", &self.handle)
            .field("pending_diffs", &self.handler.pending_diff_count())
            .finish()
    }
}
