//! Editor-backed [`ToolHandler`]: forwards tool calls to the Helix event loop
//! over the existing `McpCommand` channel (`helix-mcp-types`).
//!
//! Owns the shared plumbing (channel, registry of pending diffs, notifier)
//! and the per-client bookkeeping of T8: which CLI a proposal belongs to,
//! which one `:claude-mention` addresses (focus), replay of the cached
//! selection to each new connection.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use helix_mcp_types::{ClientInfo, DiffOutcome, McpCommand};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::clients::{ClientId, ClientSnapshot};
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
    /// Explicit `:claude-ide-focus` target; cleared when that client leaves.
    focus: Mutex<Option<ClientId>>,
    /// Show one proposal at a time (modal prompt) — `false` when the editor
    /// renders proposals as splits, which can coexist (T8 §2).
    exclusive_display: AtomicBool,
}

impl EditorHandler {
    /// Must be called on a Tokio runtime (spawns the selection task).
    pub fn new(mcp_tx: mpsc::Sender<McpCommand>) -> Self {
        let notifier: Arc<Mutex<Option<Notifier>>> = Arc::new(Mutex::new(None));
        let target = Arc::clone(&notifier);
        // `selection_changed` is broadcast to every connected CLI (T8 §2).
        let send: SendFn = Arc::new(move |method: &str, params: Value| {
            let current = target.lock().unwrap().clone();
            match current {
                Some(notifier) => notifier.notify_all(method, params) > 0,
                None => false,
            }
        });
        EditorHandler {
            mcp_tx,
            notifier,
            selection: SelectionTracker::spawn(send),
            diffs: DiffRegistry::new(),
            focus: Mutex::new(None),
            exclusive_display: AtomicBool::new(true),
        }
    }

    /// Whether `openDiff` calls queue up so that only one proposal is on
    /// screen (prompt mode, the default) or are all handed to the editor at
    /// once (split mode).
    pub fn set_exclusive_display(&self, exclusive: bool) {
        self.exclusive_display.store(exclusive, Ordering::Relaxed);
    }

    pub fn exclusive_display(&self) -> bool {
        self.exclusive_display.load(Ordering::Relaxed)
    }

    /// Proposals waiting for a decision (shown or queued), all clients.
    pub fn pending_diffs(&self) -> Vec<PendingDiff> {
        self.diffs.pending()
    }

    /// Number of `openDiff` calls waiting for a user decision, all clients.
    pub fn pending_diff_count(&self) -> usize {
        self.diffs.len()
    }

    pub fn pending_count_for(&self, client: ClientId) -> usize {
        self.diffs.count_for(client)
    }

    /// Client whose proposal the editor displayed most recently (may have
    /// disconnected since; see [`Self::active_client`] for the checked form).
    pub fn last_shown_client(&self) -> Option<ClientId> {
        self.diffs.last_shown_client()
    }

    /// Editor hook entry point: the primary selection changed or another
    /// document got focus. Debounced and de-duplicated before sending.
    pub fn selection_changed(&self, info: SelectionInfo) {
        self.selection.update(info);
    }

    /// `at_mentioned` (PROTO §6.2) to one client: insert `@path[#Lx-y]` into
    /// its prompt. `lines` are 0-indexed and inclusive. Returns `false` when
    /// that client is not connected.
    pub fn mention(&self, target: ClientId, path: &std::path::Path, lines: Option<(usize, usize)>) -> bool {
        match self.notifier() {
            Some(notifier) => notifier.notify_one(
                target,
                notify::AT_MENTIONED,
                notify::at_mentioned_params(path, lines),
            ),
            None => false,
        }
    }

    pub fn mcp_tx(&self) -> &mpsc::Sender<McpCommand> {
        &self.mcp_tx
    }

    /// Notifier of the server this handler is attached to (set on the first
    /// connection), if any.
    pub fn notifier(&self) -> Option<Notifier> {
        self.notifier.lock().unwrap().clone()
    }

    // ── clients (T8) ─────────────────────────────────────────────────────────

    /// Connected clients in connection order.
    pub fn clients(&self) -> Vec<ClientSnapshot> {
        self.notifier()
            .map(|n| n.clients())
            .unwrap_or_default()
    }

    pub fn client_count(&self) -> usize {
        self.notifier().map(|n| n.client_count()).unwrap_or(0)
    }

    pub fn client(&self, id: ClientId) -> Option<ClientSnapshot> {
        self.notifier().and_then(|n| n.client(id))
    }

    fn client_info(&self, id: ClientId) -> ClientInfo {
        self.client(id)
            .map(|c| c.info())
            .unwrap_or(ClientInfo { id: id.0, pid: None })
    }

    /// Explicit focus (`:claude-ide-focus`), if that client is still connected.
    pub fn focus(&self) -> Option<ClientId> {
        let focus = *self.focus.lock().unwrap();
        focus.filter(|id| self.client(*id).is_some())
    }

    /// Set (or clear with `None`) the explicit focus.
    pub fn set_focus(&self, target: Option<ClientId>) -> anyhow::Result<()> {
        if let Some(id) = target {
            if self.client(id).is_none() {
                anyhow::bail!("client {id} is not connected");
            }
        }
        *self.focus.lock().unwrap() = target;
        Ok(())
    }

    /// The client commands address by default (T8 §2): explicit focus, else
    /// the client whose proposal was shown last, else the most recently
    /// connected one. `None` when nobody is connected.
    pub fn active_client(&self) -> Option<ClientId> {
        if let Some(focus) = self.focus() {
            return Some(focus);
        }
        if let Some(last) = self.diffs.last_shown_client() {
            if self.client(last).is_some() {
                return Some(last);
            }
        }
        self.clients().into_iter().map(|c| c.id).max()
    }

    /// Parse a client reference typed by the user: `#N` (connection number)
    /// or a CLI pid.
    pub fn resolve_client_arg(&self, arg: &str) -> anyhow::Result<ClientId> {
        let arg = arg.trim();
        if let Some(num) = arg.strip_prefix('#') {
            let n: u64 = num
                .parse()
                .map_err(|_| anyhow::anyhow!("expected a client number like #1, got {arg:?}"))?;
            let id = ClientId(n);
            if self.client(id).is_none() {
                anyhow::bail!("no Claude Code client {id}");
            }
            return Ok(id);
        }
        let pid: u32 = arg
            .parse()
            .map_err(|_| anyhow::anyhow!("expected a pid or #N, got {arg:?}"))?;
        self.clients()
            .into_iter()
            .filter(|c| c.pid == Some(pid))
            .map(|c| c.id)
            .max()
            .ok_or_else(|| anyhow::anyhow!("no Claude Code client with pid {pid}"))
    }

    // ── tools ────────────────────────────────────────────────────────────────

    /// `openDiff` (PROTO §4.1 / §5.3): blocks until the user decides or the
    /// CLI closes the tab. Helix never writes the file — the CLI does after
    /// it receives `FILE_SAVED` (PROTO §5.2).
    async fn open_diff(&self, client: ClientId, arguments: &Value) -> anyhow::Result<ToolResult> {
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

        // T8 §2: two CLIs proposing changes to the same file is allowed — the
        // CLI writes the file after the answer, the IDE cannot arbitrate.
        if let Some(other) = self
            .diffs
            .pending()
            .iter()
            .find(|p| p.client != client && p.new_path == new_path)
        {
            log::info!(
                "claude-ide: client {client} proposes changes to {} while client {} has a pending proposal for it",
                new_path.display(),
                other.client
            );
        }

        let (reply, rx) =
            self.diffs
                .register(client, tab_name.clone(), old_path.clone(), new_path.clone());
        let command = McpCommand::OpenDiff {
            client: self.client_info(client),
            old_path,
            new_path,
            new_contents,
            tab_name: tab_name.clone(),
            reply: Arc::clone(&reply),
        };
        let outcome = if self.exclusive_display() {
            // One proposal on screen at a time; the others wait here, already
            // registered so that `close_tab` can reject them while queued.
            let _showing = self.diffs.show_lock.lock().await;
            self.show_and_wait(client, &tab_name, &reply, rx, command)
                .await?
        } else {
            self.show_and_wait(client, &tab_name, &reply, rx, command)
                .await?
        };
        self.diffs.remove(client, &tab_name);
        log::info!(
            "claude-ide: openDiff {tab_name:?} from client {client} -> {}",
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

    /// Hand the proposal to the editor (unless it was already decided while
    /// queued) and wait for the outcome.
    async fn show_and_wait(
        &self,
        client: ClientId,
        tab_name: &str,
        reply: &crate::diff::Reply,
        rx: oneshot::Receiver<DiffOutcome>,
        command: McpCommand,
    ) -> anyhow::Result<DiffOutcome> {
        let already_decided = reply.lock().unwrap().is_none();
        if !already_decided {
            self.diffs.mark_shown(client, tab_name);
            if self.mcp_tx.send(command).await.is_err() {
                self.diffs.remove(client, tab_name);
                anyhow::bail!("editor command channel closed");
            }
        }
        Ok(rx.await.unwrap_or(DiffOutcome::Rejected))
    }

    /// `close_tab {tab_name}` (PROTO §4.2): the caller's pending proposal with
    /// that name is rejected and its UI dismissed; the reply is always
    /// `TAB_CLOSED`. Another client's proposal with the same name is untouched.
    async fn close_tab(&self, client: ClientId, arguments: &Value) -> anyhow::Result<ToolResult> {
        let tab_name = arguments
            .get("tab_name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(pending) = self.diffs.resolve(client, tab_name, DiffOutcome::Rejected) {
            if pending.shown {
                self.dismiss_diff(client, pending.tab_name).await;
            }
        }
        Ok(ToolResult::text("TAB_CLOSED"))
    }

    /// `closeAllDiffTabs` (PROTO §4.3): every pending proposal *of the caller*
    /// is rejected. The count only covers the caller's proposals — the CLI
    /// sends this at the start of every turn, and it must not cancel what
    /// another CLI is waiting for (T8 §3.3).
    async fn close_all_diff_tabs(&self, client: ClientId) -> anyhow::Result<ToolResult> {
        let closed = self.diffs.resolve_all_for(client, DiffOutcome::Rejected);
        for pending in &closed {
            if pending.shown {
                self.dismiss_diff(client, pending.tab_name.clone()).await;
            }
        }
        Ok(ToolResult::text(format!(
            "CLOSED_{}_DIFF_TABS",
            closed.len()
        )))
    }

    async fn dismiss_diff(&self, client: ClientId, tab_name: String) {
        let (reply, rx) = oneshot::channel();
        if self
            .mcp_tx
            .send(McpCommand::CloseDiff {
                client: self.client_info(client),
                tab_name,
                reply,
            })
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
    async fn call(
        &self,
        client: ClientId,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<ToolResult> {
        log::debug!("claude-ide: tools/call {name} {arguments} from client {client}");
        match name {
            tools::GET_DIAGNOSTICS => self.get_diagnostics(&arguments).await,
            tools::OPEN_DIFF => self.open_diff(client, &arguments).await,
            tools::CLOSE_TAB => self.close_tab(client, &arguments).await,
            tools::CLOSE_ALL_DIFF_TABS => self.close_all_diff_tabs(client).await,
            _ => Ok(ToolResult::error(format!("{name}: not implemented"))),
        }
    }

    fn on_client_connected(&self, client: ClientId, notifier: Notifier) {
        let target = notifier.clone();
        *self.notifier.lock().unwrap() = Some(notifier);
        // PROTO §3.5: the cached selection is re-sent 500 ms after connecting —
        // to this connection only (T8 §3.4).
        let send: SendFn = Arc::new(move |method: &str, params: Value| {
            target.notify_one(client, method, params)
        });
        self.selection.replay_to(notify::REPLAY_DELAY, send);
    }

    fn on_client_disconnected(&self, client: ClientId) {
        // PROTO §5.4 / T5: a lost client cannot answer — reject *its* proposals.
        let closed = self.diffs.resolve_all_for(client, DiffOutcome::Rejected);
        self.diffs.forget_client(client);
        {
            let mut focus = self.focus.lock().unwrap();
            if *focus == Some(client) {
                *focus = None;
            }
        }
        let shown: Vec<String> = closed
            .into_iter()
            .filter(|p| p.shown)
            .map(|p| p.tab_name)
            .collect();
        if !shown.is_empty() {
            let tx = self.mcp_tx.clone();
            let info = self.client_info(client);
            tokio::spawn(async move {
                for tab_name in shown {
                    let (reply, rx) = oneshot::channel();
                    if tx
                        .send(McpCommand::CloseDiff {
                            client: info,
                            tab_name,
                            reply,
                        })
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

    /// At least one CLI is connected.
    pub fn is_connected(&self) -> bool {
        self.handle.is_connected()
    }

    pub fn client_count(&self) -> usize {
        self.handle.client_count()
    }

    pub fn clients(&self) -> Vec<ClientSnapshot> {
        self.handle.clients()
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
