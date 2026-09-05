//! WebSocket transport (PROTO §2): `127.0.0.1` only, header token check,
//! `mcp` sub-protocol echo, up to `max-clients` concurrent clients (T8), one
//! JSON-RPC message per text frame, no keepalive.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::clients::{ClientId, Clients};
use crate::jsonrpc::{self, Incoming, OutgoingNotification};
use crate::server::Dispatcher;

pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
pub const SUBPROTOCOL: &str = "mcp";
/// RFC 6455 "policy violation" — what the extension uses for a bad token.
pub const CLOSE_UNAUTHORIZED: u16 = 1008;
pub const CLOSE_NORMAL: u16 = 1000;
/// Body of the `503 Service Unavailable` answer when `max-clients` is reached.
pub const TOO_MANY_CLIENTS_BODY: &str = "too many clients";
/// How long a finished session waits for its writer to flush before aborting it.
const WRITER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Everything shared between the HTTP handler, sessions and the public handle.
pub struct Shared {
    pub auth_token: String,
    pub dispatcher: Dispatcher,
    pub clients: Arc<Clients>,
}

impl Shared {
    pub fn new(auth_token: String, dispatcher: Dispatcher, clients: Arc<Clients>) -> Arc<Self> {
        Arc::new(Shared {
            auth_token,
            dispatcher,
            clients,
        })
    }

    pub fn is_connected(&self) -> bool {
        !self.clients.is_empty()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    fn frame(method: &str, params: Value) -> Option<Message> {
        match serde_json::to_string(&OutgoingNotification::new(method, params)) {
            Ok(s) => Some(Message::Text(s.into())),
            Err(e) => {
                log::error!("claude-ide: cannot serialize notification {method}: {e}");
                None
            }
        }
    }

    /// Send a JSON-RPC notification to every connected client. Returns the
    /// number of clients it was queued for (0 when nobody is connected — the
    /// frame is dropped, like the extension does before a connection exists).
    pub fn notify_all(&self, method: &str, params: Value) -> usize {
        match Self::frame(method, params) {
            Some(msg) => self.clients.broadcast(msg),
            None => 0,
        }
    }

    /// Send a JSON-RPC notification to one client. `false` if it is gone.
    pub fn notify_one(&self, id: ClientId, method: &str, params: Value) -> bool {
        match Self::frame(method, params) {
            Some(msg) => self.clients.send_to(id, msg),
            None => false,
        }
    }

    /// Disconnect one client with a normal close. The handler is told right
    /// away (the session task finds its slot already gone and stays silent).
    pub fn close_client(&self, id: ClientId, reason: &str) -> bool {
        match self.clients.remove(id) {
            Some(conn) => {
                log::info!("claude-ide: closing client {id}: {reason}");
                conn.shutdown(reason);
                self.dispatcher.handler().on_client_disconnected(id);
                true
            }
            None => false,
        }
    }

    /// Disconnect every client (server stopping).
    pub fn close_all(&self, reason: &str) {
        for conn in self.clients.drain() {
            log::info!("claude-ide: closing client {}: {reason}", conn.id);
            conn.shutdown(reason);
            self.dispatcher.handler().on_client_disconnected(conn.id);
        }
    }
}

/// Frees a reserved client slot if the upgrade callback never runs (the
/// client vanished between the HTTP answer and the WebSocket handshake).
struct SlotGuard {
    shared: Arc<Shared>,
    id: ClientId,
    armed: bool,
}

impl SlotGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.armed && self.shared.clients.remove(self.id).is_some() {
            log::info!(
                "claude-ide: client {} never completed the upgrade; slot released",
                self.id
            );
        }
    }
}

pub fn router(shared: Arc<Shared>) -> Router {
    // Any path is accepted (PROTO §2.1): the CLI connects to `ws://127.0.0.1:<port>`.
    Router::new().fallback(upgrade).with_state(shared)
}

async fn upgrade(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
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
        log::warn!(
            "claude-ide: rejected WebSocket client {peer} with missing or wrong auth token"
        );
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

    // T8: the slot is reserved before the upgrade so that the limit check and
    // the insertion are one atomic step; over the limit the CLI gets a plain
    // HTTP 503 and keeps retrying with its own finite backoff (PROTO §2.6).
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Message>();
    let (close_tx, close_rx) = watch::channel(false);
    let id = match shared.clients.try_insert(out_tx.clone(), close_tx) {
        Ok(id) => id,
        Err(e) => {
            log::warn!("claude-ide: refusing WebSocket client {peer}: {e}");
            return (StatusCode::SERVICE_UNAVAILABLE, TOO_MANY_CLIENTS_BODY).into_response();
        }
    };
    let guard = SlotGuard {
        shared: Arc::clone(&shared),
        id,
        armed: true,
    };
    ws.on_upgrade(move |socket| run_session(socket, shared, id, out_tx, out_rx, close_rx, guard))
}

async fn run_session(
    socket: WebSocket,
    shared: Arc<Shared>,
    id: ClientId,
    out_tx: mpsc::UnboundedSender<Message>,
    mut out_rx: mpsc::UnboundedReceiver<Message>,
    mut close_rx: watch::Receiver<bool>,
    mut guard: SlotGuard,
) {
    // From here on this task owns the slot's lifecycle.
    guard.disarm();
    log::info!(
        "claude-ide: client {id} connected ({} of {})",
        shared.client_count(),
        shared.clients.max_clients()
    );
    shared
        .dispatcher
        .handler()
        .on_client_connected(id, crate::Notifier::new(Arc::clone(&shared)));

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
                    handle_text(&shared, id, &out_tx, text.as_str());
                }
                Some(Ok(Message::Close(frame))) => {
                    // The WebSocket layer already queued the close reply.
                    log::info!("claude-ide: client {id} closed ({frame:?})");
                    break;
                }
                // Binary frames are not part of the protocol; ping/pong are
                // answered by the WebSocket layer itself.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    log::info!("claude-ide: client {id} socket error: {e}");
                    break;
                }
                None => break,
            },
        }
    }

    // Drop our slot first: the `ClientConn` inside holds an `out_tx` clone,
    // and the writer only finishes once every sender is gone. `None` means
    // `close_client`/`close_all` already removed it and told the handler.
    let was_present = shared.clients.remove(id).is_some();
    drop(out_tx);
    let abort = writer.abort_handle();
    if tokio::time::timeout(WRITER_GRACE, writer).await.is_err() {
        abort.abort();
    }
    if was_present {
        log::info!(
            "claude-ide: client {id} disconnected ({} left)",
            shared.client_count()
        );
        shared.dispatcher.handler().on_client_disconnected(id);
    }
}

fn handle_text(
    shared: &Arc<Shared>,
    id: ClientId,
    out_tx: &mpsc::UnboundedSender<Message>,
    text: &str,
) {
    match jsonrpc::parse(text) {
        Ok(Incoming::Request(req)) => {
            log::debug!("claude-ide: {id} <- {} (id {})", req.method, req.id);
            // Each request runs on its own task so a blocking `openDiff`
            // never delays `close_tab` / `closeAllDiffTabs` (PROTO §5.4).
            let dispatcher = shared.dispatcher.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = dispatcher.handle_request(id, req).await;
                log::debug!(
                    "claude-ide: {id} -> id {} {}",
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
            log::debug!("claude-ide: {id} <- notification {}", note.method);
            shared.dispatcher.handle_notification(id, note)
        }
        Ok(Incoming::Response(rid)) => {
            log::debug!("claude-ide: {id} ignoring unexpected response for id {rid}");
        }
        // The extension reports parse errors via `onerror` and keeps the
        // connection open; we do the same.
        Err(e) => log::warn!("claude-ide: {id} dropping unparseable frame: {e}"),
    }
}
