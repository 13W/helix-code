//! Editor-backed [`ToolHandler`]: forwards tool calls to the Helix event loop
//! over the existing `McpCommand` channel (`helix-mcp-types`).
//!
//! The individual tools are filled in by later tasks; this module owns the
//! shared plumbing (channel, registry of pending diffs, notifier).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use helix_mcp_types::{DiffOutcome, McpCommand};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::diff::{DiffRegistry, PendingDiff};
use crate::notify::{self, SelectionInfo, SelectionTracker, SendFn};
use crate::tools::{self, ToolHandler, ToolResult};
use crate::{diagnostics, IdeServerHandle, Notifier};

/// Tool handler wired to the editor.
pub struct EditorHandler {
    mcp_tx: mpsc::Sender<McpCommand>,
    notifier: Arc<Mutex<Option<Notifier>>>,
    selection: SelectionTracker,
    diffs: DiffRegistry,
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
            diffs: DiffRegistry::new(),
        }
    }

    /// Proposals waiting for a decision (shown or queued).
    pub fn pending_diffs(&self) -> Vec<PendingDiff> {
        self.diffs.pending()
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
        self.diffs.len()
    }

    /// `openDiff` (PROTO §4.1 / §5.3): blocks until the user decides or the
    /// CLI closes the tab. Helix never writes the file — the CLI does after
    /// it receives `FILE_SAVED` (PROTO §5.2).
    async fn open_diff(&self, arguments: &Value) -> anyhow::Result<ToolResult> {
        let field = |name: &str| -> anyhow::Result<String> {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("missing {name}"))
        };
        let old_path = PathBuf::from(field("old_file_path")?);
        let new_path = PathBuf::from(field("new_file_path")?);
        let new_contents = field("new_file_contents")?;
        let tab_name = field("tab_name")?;

        let (reply, rx) = self
            .diffs
            .register(tab_name.clone(), old_path.clone(), new_path.clone());
        let outcome = {
            // One proposal on screen at a time; the others wait here, already
            // registered so that `close_tab` can reject them while queued.
            let _showing = self.diffs.show_lock.lock().await;
            let already_decided = reply.lock().unwrap().is_none();
            if !already_decided {
                self.diffs.mark_shown(&tab_name);
                let sent = self
                    .mcp_tx
                    .send(McpCommand::OpenDiff {
                        old_path,
                        new_path,
                        new_contents,
                        tab_name: tab_name.clone(),
                        reply: Arc::clone(&reply),
                    })
                    .await;
                if sent.is_err() {
                    self.diffs.remove(&tab_name);
                    anyhow::bail!("editor command channel closed");
                }
            }
            rx.await.unwrap_or(DiffOutcome::Rejected)
        };
        self.diffs.remove(&tab_name);
        log::info!(
            "claude-ide: openDiff {tab_name:?} -> {}",
            match &outcome {
                DiffOutcome::Saved(_) => "FILE_SAVED",
                DiffOutcome::Rejected => "DIFF_REJECTED",
            }
        );
        Ok(match outcome {
            DiffOutcome::Saved(contents) => ToolResult::texts(["FILE_SAVED".to_string(), contents]),
            DiffOutcome::Rejected => ToolResult::texts(["DIFF_REJECTED".to_string(), tab_name]),
        })
    }

    /// `close_tab {tab_name}` (PROTO §4.2): a pending proposal with that name
    /// is rejected and its UI dismissed; the reply is always `TAB_CLOSED`.
    async fn close_tab(&self, arguments: &Value) -> anyhow::Result<ToolResult> {
        let tab_name = arguments
            .get("tab_name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(pending) = self.diffs.resolve(tab_name, DiffOutcome::Rejected) {
            if pending.shown {
                self.dismiss_diff(pending.tab_name).await;
            }
        }
        Ok(ToolResult::text("TAB_CLOSED"))
    }

    /// `closeAllDiffTabs` (PROTO §4.3): every pending proposal is rejected.
    async fn close_all_diff_tabs(&self) -> anyhow::Result<ToolResult> {
        let closed = self.diffs.resolve_all(DiffOutcome::Rejected);
        for pending in &closed {
            if pending.shown {
                self.dismiss_diff(pending.tab_name.clone()).await;
            }
        }
        Ok(ToolResult::text(format!(
            "CLOSED_{}_DIFF_TABS",
            closed.len()
        )))
    }

    async fn dismiss_diff(&self, tab_name: String) {
        let (reply, rx) = oneshot::channel();
        if self
            .mcp_tx
            .send(McpCommand::CloseDiff { tab_name, reply })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
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
            tools::OPEN_DIFF => self.open_diff(&arguments).await,
            tools::CLOSE_TAB => self.close_tab(&arguments).await,
            tools::CLOSE_ALL_DIFF_TABS => self.close_all_diff_tabs().await,
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
        // PROTO §5.4 / T5: a lost client cannot answer, reject everything.
        let closed = self.diffs.resolve_all(DiffOutcome::Rejected);
        let shown: Vec<String> = closed
            .into_iter()
            .filter(|p| p.shown)
            .map(|p| p.tab_name)
            .collect();
        if !shown.is_empty() {
            let tx = self.mcp_tx.clone();
            tokio::spawn(async move {
                for tab_name in shown {
                    let (reply, rx) = oneshot::channel();
                    if tx
                        .send(McpCommand::CloseDiff { tab_name, reply })
                        .await
                        .is_ok()
                    {
                        let _ = rx.await;
                    }
                }
            });
        }
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
