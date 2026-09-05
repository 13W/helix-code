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

pub mod clients;
pub mod diagnostics;
pub mod diff;
pub mod handler;
pub mod jsonrpc;
pub mod lockfile;
pub mod notify;
pub mod port;
pub mod procinfo;
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

pub use clients::{ClientId, ClientInfo, ClientSnapshot, Clients, DEFAULT_MAX_CLIENTS};
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
    /// How many CLIs may be connected at once (T8); further upgrades get HTTP 503.
    pub max_clients: usize,
}

impl Config {
    pub fn new(workspace: impl Into<PathBuf>, ide_name: impl Into<String>) -> Self {
        Config {
            workspace_folders: vec![workspace.into()],
            ide_name: ide_name.into(),
            pid: std::process::id(),
            fixed_port: None,
            lock_dir: None,
            max_clients: DEFAULT_MAX_CLIENTS,
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

    /// Send `{"jsonrpc":"2.0","method":..,"params":..}` to every connected
    /// client. Returns `false` if nobody is connected.
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.notify_all(method, params) > 0
    }

    /// Broadcast; returns the number of clients the frame was queued for.
    pub fn notify_all(&self, method: &str, params: Value) -> usize {
        self.shared.notify_all(method, params)
    }

    /// Send to one client; `false` if it is gone.
    pub fn notify_one(&self, client: ClientId, method: &str, params: Value) -> bool {
        self.shared.notify_one(client, method, params)
    }

    pub fn is_connected(&self) -> bool {
        self.shared.is_connected()
    }

    pub fn client_count(&self) -> usize {
        self.shared.client_count()
    }

    pub fn clients(&self) -> Vec<ClientSnapshot> {
        self.shared.clients.snapshots()
    }

    pub fn client(&self, id: ClientId) -> Option<ClientSnapshot> {
        self.shared.clients.snapshot(id)
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("clients", &self.client_count())
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

    /// At least one client is connected.
    pub fn is_connected(&self) -> bool {
        self.inner.shared.is_connected()
    }

    pub fn client_count(&self) -> usize {
        self.inner.shared.client_count()
    }

    pub fn max_clients(&self) -> usize {
        self.inner.shared.clients.max_clients()
    }

    /// Connected clients in connection order.
    pub fn clients(&self) -> Vec<ClientSnapshot> {
        self.inner.shared.clients.snapshots()
    }

    pub fn client(&self, id: ClientId) -> Option<ClientSnapshot> {
        self.inner.shared.clients.snapshot(id)
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    pub fn notifier(&self) -> Notifier {
        Notifier::new(Arc::clone(&self.inner.shared))
    }

    /// See [`Notifier::notify`] (broadcast).
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.inner.shared.notify_all(method, params) > 0
    }

    /// See [`Notifier::notify_all`].
    pub fn notify_all(&self, method: &str, params: Value) -> usize {
        self.inner.shared.notify_all(method, params)
    }

    /// See [`Notifier::notify_one`].
    pub fn notify_one(&self, client: ClientId, method: &str, params: Value) -> bool {
        self.inner.shared.notify_one(client, method, params)
    }

    /// Disconnect one client with close code 1000 and `reason`
    /// (`:claude-ide-disconnect`). The CLI will try to reconnect a few times
    /// (PROTO §2.6 as observed). `false` if no such client.
    pub fn close_client(&self, client: ClientId, reason: &str) -> bool {
        self.inner.shared.close_client(client, reason)
    }

    /// Remove the lock file without stopping the server. Safe to call from a
    /// panic hook or signal handler; idempotent.
    pub fn remove_lock_file(&self) {
        if let Err(e) = lockfile::remove(&self.inner.lock_path) {
            log::warn!("claude-ide: cannot remove lock file: {e}");
        }
    }

    /// Disconnect every client, stop accepting connections and delete the
    /// lock file. Idempotent.
    pub async fn stop(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.shared.close_all("IDE server stopping");
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
            .field("clients", &self.client_count())
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// Bind the server, write the lock file and start accepting connections.
///
/// Must be called from within a Tokio runtime; the accept loop is spawned on it.
pub async fn start(config: Config, handler: SharedHandler) -> anyhow::Result<IdeServerHandle> {
    if config.max_clients == 0 {
        anyhow::bail!("max-clients must be at least 1");
    }
    let listener = port::bind(config.fixed_port).await?;
    let port = listener.local_addr()?.port();

    let auth_token = uuid::Uuid::new_v4().to_string();
    let clients = Arc::new(Clients::new(config.max_clients));
    let shared = transport::Shared::new(
        auth_token.clone(),
        Dispatcher::new(handler, Arc::clone(&clients)),
        clients,
    );
    let app = transport::router(Arc::clone(&shared))
        .into_make_service_with_connect_info::<std::net::SocketAddr>();

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
        "claude-ide: listening on ws://127.0.0.1:{port} as {:?} (max {} clients), lock file {}",
        config.ide_name,
        config.max_clients,
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
