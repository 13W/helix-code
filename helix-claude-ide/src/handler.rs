//! Editor-backed [`ToolHandler`]: forwards tool calls to the Helix event loop
//! over the existing `McpCommand` channel (`helix-mcp-types`).
//!
//! The individual tools are filled in by later tasks; this module owns the
//! shared plumbing (channel, registry of pending diffs, notifier).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use helix_mcp_types::McpCommand;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::notify::{self, SelectionInfo, SelectionTracker, SendFn};
use crate::tools::{self, ToolHandler, ToolResult};
use crate::{diagnostics, IdeServerHandle, Notifier};

/// Tool handler wired to the editor.
pub struct EditorHandler {
    mcp_tx: mpsc::Sender<McpCommand>,
    notifier: Arc<Mutex<Option<Notifier>>>,
    selection: SelectionTracker,
}

impl EditorHandler {
    /// Must be called on a Tokio runtime (spawns the selection task).
    pub fn new(mcp_tx: mpsc::Sender<McpCommand>) -> Self {
        let notifier: Arc<Mutex<Option<Notifier>>> = Arc::new(Mutex::new(None));
        let target = Arc::clone(&notifier);
        let send: SendFn = Arc::new(move |method: &str, params: Value| {
            let current = target.lock().unwrap().clone();
            match current {
                Some(notifier) => notifier.notify(method, params),
                None => false,
            }
        });
        EditorHandler {
            mcp_tx,
            notifier,
            selection: SelectionTracker::spawn(send),
        }
    }

    /// Editor hook entry point: the primary selection changed or another
    /// document got focus. Debounced and de-duplicated before sending.
    pub fn selection_changed(&self, info: SelectionInfo) {
        self.selection.update(info);
    }

    /// `at_mentioned` (PROTO §6.2): insert `@path[#Lx-y]` into the CLI
    /// prompt. `lines` are 0-indexed and inclusive. Returns `false` when no
    /// client is connected.
    pub fn mention(&self, path: &std::path::Path, lines: Option<(usize, usize)>) -> bool {
        match self.notifier() {
            Some(notifier) => notifier.notify(
                notify::AT_MENTIONED,
                notify::at_mentioned_params(path, lines),
            ),
            None => false,
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

    /// `getDiagnostics {uri?}` (PROTO §4.4). The CLI enforces its own
    /// 500 ms / 2 s budgets, so the editor round-trip is not timed out here.
    async fn get_diagnostics(&self, arguments: &Value) -> anyhow::Result<ToolResult> {
        let path = match arguments.get("uri").and_then(Value::as_str) {
            Some(uri) => Some(diagnostics::uri_to_path(uri)?),
            None => None,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.mcp_tx
            .send(McpCommand::GetDiagnostics {
                path,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("editor command channel closed"))?;
        let files = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor did not reply"))?;
        Ok(ToolResult::text(diagnostics::render(files)))
    }
}

#[async_trait]
impl ToolHandler for EditorHandler {
    async fn call(&self, name: &str, arguments: Value) -> anyhow::Result<ToolResult> {
        log::debug!("claude-ide: tools/call {name} {arguments}");
        match name {
            tools::GET_DIAGNOSTICS => self.get_diagnostics(&arguments).await,
            _ => Ok(ToolResult::error(format!("{name}: not implemented"))),
        }
    }

    fn on_client_connected(&self, notifier: Notifier) {
        *self.notifier.lock().unwrap() = Some(notifier);
        // PROTO §3.5: the cached selection is re-sent 500 ms after connecting.
        self.selection.replay_after(notify::REPLAY_DELAY);
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
