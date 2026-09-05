//! Claude Code IDE integration for Helix.
//!
//! Implements the IDE side of the protocol that the `claude` CLI (2.1.261)
//! speaks to editors: a loopback WebSocket MCP server discovered through a
//! lock file in `~/.claude/ide`. See `claude-code-ide-protocol-spec.md` in
//! the repository root for the protocol as extracted from the official
//! binaries.
//!
//! This crate knows nothing about the editor: it owns the transport, the
//! lock file and the JSON-RPC dispatch, and delegates tool calls to a
//! [`ToolHandler`] supplied by the embedding application.

pub mod diagnostics;
pub mod handler;
pub mod jsonrpc;
pub mod lockfile;
pub mod port;
pub mod server;
pub mod tools;
pub mod transport;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use handler::{EditorHandler, Session};
pub use lockfile::LockFile;
pub use server::Dispatcher;
pub use tools::{Content, NotImplementedHandler, SharedHandler, ToolHandler, ToolResult};

/// How long `stop()` waits for the accept loop to wind down before aborting it.
const STOP_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Config {
    /// Absolute paths advertised in the lock file. The CLI only connects if its
    /// cwd equals one of them or lies underneath (PROTO §1.3).
    pub workspace_folders: Vec<PathBuf>,
    /// Shown by `/ide`. Not required to be unique.
    pub ide_name: String,
    /// Pid recorded in the lock file; the CLI probes it with `kill(pid, 0)`.
    pub pid: u32,
    /// Bind this port instead of picking a random one.
    pub fixed_port: Option<u16>,
    /// Override the lock directory (tests); `None` resolves per PROTO §1.1.
    pub lock_dir: Option<PathBuf>,
}

impl Config {
    pub fn new(workspace: impl Into<PathBuf>, ide_name: impl Into<String>) -> Self {
        Config {
            workspace_folders: vec![workspace.into()],
            ide_name: ide_name.into(),
            pid: std::process::id(),
            fixed_port: None,
            lock_dir: None,
        }
    }
}

/// Cheap, cloneable sender for IDE → CLI notifications (PROTO §6).
#[derive(Clone)]
pub struct Notifier {
    shared: Arc<transport::Shared>,
}

impl Notifier {
    pub(crate) fn new(shared: Arc<transport::Shared>) -> Self {
        Notifier { shared }
    }

    /// Send `{"jsonrpc":"2.0","method":..,"params":..}` to the connected
    /// client. Returns `false` if nobody is connected.
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.shared.notify(method, params)
    }

    pub fn is_connected(&self) -> bool {
        self.shared.is_connected()
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("connected", &self.is_connected())
            .finish()
    }
}

struct Inner {
    port: u16,
    lock_path: PathBuf,
    shared: Arc<transport::Shared>,
    shutdown: watch::Sender<bool>,
    server_task: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Best effort: a dropped handle must not leave a stale lock behind.
        if !self.stopped.load(Ordering::SeqCst) {
            let _ = self.shutdown.send(true);
            let _ = lockfile::remove(&self.lock_path);
        }
    }
}

/// Handle to a running IDE server. Cloneable; the server stops when `stop()`
/// is called or the last clone is dropped.
#[derive(Clone)]
pub struct IdeServerHandle {
    inner: Arc<Inner>,
}

impl IdeServerHandle {
    pub fn port(&self) -> u16 {
        self.inner.port
    }

    pub fn lock_path(&self) -> &Path {
        &self.inner.lock_path
    }

    pub fn is_connected(&self) -> bool {
        self.inner.shared.is_connected()
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    pub fn notifier(&self) -> Notifier {
        Notifier::new(Arc::clone(&self.inner.shared))
    }

    /// See [`Notifier::notify`].
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.inner.shared.notify(method, params)
    }

    /// Remove the lock file without stopping the server. Safe to call from a
    /// panic hook or signal handler; idempotent.
    pub fn remove_lock_file(&self) {
        if let Err(e) = lockfile::remove(&self.inner.lock_path) {
            log::warn!("claude-ide: cannot remove lock file: {e}");
        }
    }

    /// Disconnect the client, stop accepting connections and delete the lock
    /// file. Idempotent.
    pub async fn stop(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.shared.close_client("IDE server stopping");
        let _ = self.inner.shutdown.send(true);
        let task = self.inner.server_task.lock().unwrap().take();
        if let Some(task) = task {
            let abort = task.abort_handle();
            if tokio::time::timeout(STOP_GRACE, task).await.is_err() {
                log::warn!("claude-ide: accept loop did not stop in time; aborting");
                abort.abort();
            }
        }
        self.remove_lock_file();
    }
}

impl std::fmt::Debug for IdeServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdeServerHandle")
            .field("port", &self.inner.port)
            .field("lock_path", &self.inner.lock_path)
            .field("connected", &self.is_connected())
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// Bind the server, write the lock file and start accepting connections.
///
/// Must be called from within a Tokio runtime; the accept loop is spawned on it.
pub async fn start(config: Config, handler: SharedHandler) -> anyhow::Result<IdeServerHandle> {
    let listener = port::bind(config.fixed_port).await?;
    let port = listener.local_addr()?.port();

    let auth_token = uuid::Uuid::new_v4().to_string();
    let shared = transport::Shared::new(auth_token.clone(), Dispatcher::new(handler));
    let app = transport::router(Arc::clone(&shared));

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        // Ends when `true` is sent or the sender is dropped.
        while shutdown_rx.changed().await.is_ok() {
            if *shutdown_rx.borrow() {
                break;
            }
        }
    });
    let server_task = tokio::spawn(async move {
        if let Err(e) = server.await {
            log::error!("claude-ide: server error: {e}");
        }
    });

    // Written only after a successful bind, like the extension's `listening` handler.
    let dir = lockfile::lock_dir(config.lock_dir.as_deref());
    let lock = LockFile::new(
        config.pid,
        &config.workspace_folders,
        config.ide_name.clone(),
        auth_token,
    );
    let lock_path = match lockfile::write(&dir, port, &lock) {
        Ok(path) => path,
        Err(e) => {
            let _ = shutdown_tx.send(true);
            server_task.abort();
            return Err(anyhow::anyhow!(
                "cannot write lock file in {}: {e}",
                dir.display()
            ));
        }
    };
    log::info!(
        "claude-ide: listening on ws://127.0.0.1:{port} as {:?}, lock file {}",
        config.ide_name,
        lock_path.display()
    );

    Ok(IdeServerHandle {
        inner: Arc::new(Inner {
            port,
            lock_path,
            shared,
            shutdown: shutdown_tx,
            server_task: Mutex::new(Some(server_task)),
            stopped: AtomicBool::new(false),
        }),
    })
}
