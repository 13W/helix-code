//! WebSocket transport (PROTO §2): `127.0.0.1` only, header token check,
//! `mcp` sub-protocol echo, a single client at a time, one JSON-RPC message
//! per text frame, no keepalive.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::jsonrpc::{self, Incoming, OutgoingNotification};
use crate::server::Dispatcher;

pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
pub const SUBPROTOCOL: &str = "mcp";
/// RFC 6455 "policy violation" — what the extension uses for a bad token.
pub const CLOSE_UNAUTHORIZED: u16 = 1008;
pub const CLOSE_NORMAL: u16 = 1000;
/// How long a finished session waits for its writer to flush before aborting it.
const WRITER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Everything shared between the HTTP handler, sessions and the public handle.
pub struct Shared {
    pub auth_token: String,
    pub dispatcher: Dispatcher,
    client: Mutex<Option<ClientConn>>,
    next_client_id: AtomicU64,
}

struct ClientConn {
    id: u64,
    out_tx: mpsc::UnboundedSender<Message>,
    /// Flipped to `true` to make the session's read loop exit.
    close: watch::Sender<bool>,
}

impl Shared {
    pub fn new(auth_token: String, dispatcher: Dispatcher) -> Arc<Self> {
        Arc::new(Shared {
            auth_token,
            dispatcher,
            client: Mutex::new(None),
            next_client_id: AtomicU64::new(1),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.client.lock().unwrap().is_some()
    }

    /// Send a JSON-RPC notification to the current client. Returns `false`
    /// when no client is connected (the frame is dropped, like the extension
    /// does before a connection exists).
    pub fn notify(&self, method: &str, params: Value) -> bool {
        let frame = match serde_json::to_string(&OutgoingNotification::new(method, params)) {
            Ok(s) => s,
            Err(e) => {
                log::error!("claude-ide: cannot serialize notification {method}: {e}");
                return false;
            }
        };
        let guard = self.client.lock().unwrap();
        match guard.as_ref() {
            Some(client) => client.out_tx.send(Message::Text(frame.into())).is_ok(),
            None => false,
        }
    }

    /// Disconnect the current client (if any) with a normal close.
    pub fn close_client(&self, reason: &str) {
        let prev = self.client.lock().unwrap().take();
        if let Some(prev) = prev {
            prev.shutdown(reason);
            self.dispatcher.handler().on_client_disconnected();
        }
    }

    /// Register a new client, evicting the previous one (PROTO §2.3).
    fn install_client(&self, conn: ClientConn) {
        let prev = self.client.lock().unwrap().replace(conn);
        if let Some(prev) = prev {
            log::info!("claude-ide: disconnecting previous WebSocket client");
            prev.shutdown("Replaced by a new client");
            self.dispatcher.handler().on_client_disconnected();
        }
    }

    /// Clear the slot only if it still holds `id` (a newer client may have
    /// replaced us already).
    fn remove_client(&self, id: u64) -> bool {
        let mut guard = self.client.lock().unwrap();
        if guard.as_ref().map(|c| c.id) == Some(id) {
            *guard = None;
            true
        } else {
            false
        }
    }
}

impl ClientConn {
    fn shutdown(&self, reason: &str) {
        let _ = self.out_tx.send(Message::Close(Some(CloseFrame {
            code: CLOSE_NORMAL,
            reason: reason.to_string().into(),
        })));
        let _ = self.close.send(true);
    }
}

pub fn router(shared: Arc<Shared>) -> Router {
    // Any path is accepted (PROTO §2.1): the CLI connects to `ws://127.0.0.1:<port>`.
    Router::new().fallback(upgrade).with_state(shared)
}

async fn upgrade(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ws = ws.protocols([SUBPROTOCOL]);
    let authorized = headers
        .get(AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|token| token == shared.auth_token)
        .unwrap_or(false);
    if !authorized {
        log::warn!("claude-ide: rejected WebSocket client with missing or wrong auth token");
        // The extension completes the upgrade and then closes with 1008.
        return ws.on_upgrade(|mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: CLOSE_UNAUTHORIZED,
                    reason: "Unauthorized".into(),
                })))
                .await;
        });
    }
    ws.on_upgrade(move |socket| run_session(socket, shared))
}

async fn run_session(socket: WebSocket, shared: Arc<Shared>) {
    let id = shared.next_client_id.fetch_add(1, Ordering::Relaxed);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let (close_tx, mut close_rx) = watch::channel(false);
    shared.install_client(ClientConn {
        id,
        out_tx: out_tx.clone(),
        close: close_tx,
    });
    log::info!("claude-ide: client #{id} connected");
    shared
        .dispatcher
        .handler()
        .on_client_connected(crate::Notifier::new(Arc::clone(&shared)));

    let (mut sink, mut stream) = socket.split();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let is_close = matches!(msg, Message::Close(_));
            if let Err(e) = sink.send(msg).await {
                log::debug!("claude-ide: send failed: {e}");
                break;
            }
            if is_close {
                break;
            }
        }
        // Flushes a pending close reply or sends our own close frame.
        let _ = sink.close().await;
    });

    loop {
        tokio::select! {
            _ = close_rx.changed() => break,
            next = stream.next() => match next {
                Some(Ok(Message::Text(text))) => {
                    handle_text(&shared, &out_tx, text.as_str());
                }
                Some(Ok(Message::Close(frame))) => {
                    // The WebSocket layer already queued the close reply.
                    log::info!("claude-ide: client #{id} closed ({frame:?})");
                    break;
                }
                // Binary frames are not part of the protocol; ping/pong are
                // answered by the WebSocket layer itself.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    log::info!("claude-ide: client #{id} socket error: {e}");
                    break;
                }
                None => break,
            },
        }
    }

    // Drop our slot first: the `ClientConn` inside holds an `out_tx` clone,
    // and the writer only finishes once every sender is gone.
    let was_current = shared.remove_client(id);
    drop(out_tx);
    let abort = writer.abort_handle();
    if tokio::time::timeout(WRITER_GRACE, writer).await.is_err() {
        abort.abort();
    }
    if was_current {
        log::info!("claude-ide: client #{id} disconnected");
        shared.dispatcher.handler().on_client_disconnected();
    }
}

fn handle_text(shared: &Arc<Shared>, out_tx: &mpsc::UnboundedSender<Message>, text: &str) {
    match jsonrpc::parse(text) {
        Ok(Incoming::Request(req)) => {
            log::debug!("claude-ide: <- {} (id {})", req.method, req.id);
            // Each request runs on its own task so a blocking `openDiff`
            // never delays `close_tab` / `closeAllDiffTabs` (PROTO §5.4).
            let dispatcher = shared.dispatcher.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = dispatcher.handle_request(req).await;
                log::debug!(
                    "claude-ide: -> id {} {}",
                    response.id,
                    if response.error.is_some() {
                        "error"
                    } else {
                        "ok"
                    }
                );
                match serde_json::to_string(&response) {
                    Ok(frame) => {
                        let _ = out_tx.send(Message::Text(frame.into()));
                    }
                    Err(e) => log::error!("claude-ide: cannot serialize response: {e}"),
                }
            });
        }
        Ok(Incoming::Notification(note)) => {
            log::debug!("claude-ide: <- notification {}", note.method);
            shared.dispatcher.handle_notification(note)
        }
        Ok(Incoming::Response(id)) => {
            log::debug!("claude-ide: ignoring unexpected response for id {id}");
        }
        // The extension reports parse errors via `onerror` and keeps the
        // connection open; we do the same.
        Err(e) => log::warn!("claude-ide: dropping unparseable frame: {e}"),
    }
}
