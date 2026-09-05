//! Connected CLI clients (T8): identity, per-client state and fan-out.
//!
//! The server accepts up to `max_clients` concurrent WebSocket connections
//! instead of evicting the previous one (the VS Code extension's behaviour,
//! PROTO §2.3, which combined with the CLI's auto-reconnect made two CLIs in
//! one workspace evict each other in a loop).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use axum::extract::ws::{CloseFrame, Message};
use tokio::sync::{mpsc, watch};

pub use helix_mcp_types::ClientInfo;

use crate::transport::CLOSE_NORMAL;

/// Default for `max-clients`.
pub const DEFAULT_MAX_CLIENTS: usize = 4;

/// Sequential number of a WebSocket connection (`client #N` in the logs).
/// A CLI that reconnects gets a new id but keeps its pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(pub u64);

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Read-only view of one connected client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSnapshot {
    pub id: ClientId,
    /// From `ide_connected {pid}` (PROTO §3.4); `None` until that notification arrives.
    pub pid: Option<u32>,
    pub connected_at: Instant,
    /// Last `set_permission_mode {mode}` (PROTO §4.5), e.g. `default`, `plan`, `acceptEdits`.
    pub permission_mode: Option<String>,
}

impl ClientSnapshot {
    pub fn info(&self) -> ClientInfo {
        ClientInfo {
            id: self.id.0,
            pid: self.pid,
        }
    }
}

pub(crate) struct ClientConn {
    pub id: ClientId,
    pub out_tx: mpsc::UnboundedSender<Message>,
    /// Flipped to `true` to make the session's read loop exit.
    pub close: watch::Sender<bool>,
    pub pid: Option<u32>,
    pub connected_at: Instant,
    pub permission_mode: Option<String>,
}

impl ClientConn {
    pub(crate) fn shutdown(&self, reason: &str) {
        let _ = self.out_tx.send(Message::Close(Some(CloseFrame {
            code: CLOSE_NORMAL,
            reason: reason.to_string().into(),
        })));
        let _ = self.close.send(true);
    }

    fn snapshot(&self) -> ClientSnapshot {
        ClientSnapshot {
            id: self.id,
            pid: self.pid,
            connected_at: self.connected_at,
            permission_mode: self.permission_mode.clone(),
        }
    }
}

/// The connection limit is reached; the upgrade is refused with HTTP 503.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyClients {
    pub max: usize,
}

impl std::fmt::Display for TooManyClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "too many clients (max-clients = {})", self.max)
    }
}

impl std::error::Error for TooManyClients {}

/// Table of connected clients, shared by the transport (fan-out, lifecycle)
/// and the dispatcher (`ide_connected`, `set_permission_mode`).
pub struct Clients {
    map: Mutex<HashMap<ClientId, ClientConn>>,
    next_id: AtomicU64,
    max_clients: usize,
}

impl Clients {
    pub fn new(max_clients: usize) -> Self {
        Clients {
            map: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            max_clients: max_clients.max(1),
        }
    }

    pub fn max_clients(&self) -> usize {
        self.max_clients
    }

    /// Reserve a slot for a new connection, atomically with the limit check.
    pub(crate) fn try_insert(
        &self,
        out_tx: mpsc::UnboundedSender<Message>,
        close: watch::Sender<bool>,
    ) -> Result<ClientId, TooManyClients> {
        let mut map = self.map.lock().unwrap();
        if map.len() >= self.max_clients {
            return Err(TooManyClients {
                max: self.max_clients,
            });
        }
        let id = ClientId(self.next_id.fetch_add(1, Ordering::Relaxed));
        map.insert(
            id,
            ClientConn {
                id,
                out_tx,
                close,
                pid: None,
                connected_at: Instant::now(),
                permission_mode: None,
            },
        );
        Ok(id)
    }

    pub(crate) fn remove(&self, id: ClientId) -> Option<ClientConn> {
        self.map.lock().unwrap().remove(&id)
    }

    pub(crate) fn drain(&self) -> Vec<ClientConn> {
        let mut conns: Vec<ClientConn> = self.map.lock().unwrap().drain().map(|(_, c)| c).collect();
        conns.sort_by_key(|c| c.id);
        conns
    }

    /// Record the CLI pid from `ide_connected`. `false` if the client is gone.
    pub fn set_pid(&self, id: ClientId, pid: u32) -> bool {
        match self.map.lock().unwrap().get_mut(&id) {
            Some(conn) => {
                conn.pid = Some(pid);
                true
            }
            None => false,
        }
    }

    pub fn set_permission_mode(&self, id: ClientId, mode: impl Into<String>) -> bool {
        match self.map.lock().unwrap().get_mut(&id) {
            Some(conn) => {
                conn.permission_mode = Some(mode.into());
                true
            }
            None => false,
        }
    }

    pub fn snapshot(&self, id: ClientId) -> Option<ClientSnapshot> {
        self.map.lock().unwrap().get(&id).map(ClientConn::snapshot)
    }

    /// All connected clients, ordered by id (connection order).
    pub fn snapshots(&self) -> Vec<ClientSnapshot> {
        let mut all: Vec<ClientSnapshot> = self
            .map
            .lock()
            .unwrap()
            .values()
            .map(ClientConn::snapshot)
            .collect();
        all.sort_by_key(|c| c.id);
        all
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, id: ClientId) -> bool {
        self.map.lock().unwrap().contains_key(&id)
    }

    /// The (newest) connection that reported `pid`.
    pub fn find_by_pid(&self, pid: u32) -> Option<ClientId> {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.pid == Some(pid))
            .map(|c| c.id)
            .max()
    }

    pub(crate) fn send_to(&self, id: ClientId, msg: Message) -> bool {
        match self.map.lock().unwrap().get(&id) {
            Some(conn) => conn.out_tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// Send `msg` to every client; a failed send to one does not stop the
    /// others. Returns how many sends succeeded.
    pub(crate) fn broadcast(&self, msg: Message) -> usize {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter(|conn| conn.out_tx.send(msg.clone()).is_ok())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> (
        mpsc::UnboundedSender<Message>,
        mpsc::UnboundedReceiver<Message>,
        watch::Sender<bool>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (close, _) = watch::channel(false);
        (tx, rx, close)
    }

    #[test]
    fn limit_is_enforced_and_slots_are_reused() {
        let clients = Clients::new(2);
        let (t1, _r1, c1) = conn();
        let (t2, _r2, c2) = conn();
        let (t3, _r3, c3) = conn();
        let a = clients.try_insert(t1, c1).unwrap();
        let b = clients.try_insert(t2, c2).unwrap();
        assert_eq!((a, b), (ClientId(1), ClientId(2)));
        assert_eq!(
            clients.try_insert(t3.clone(), c3).unwrap_err(),
            TooManyClients { max: 2 }
        );
        assert!(clients.remove(a).is_some());
        let (_, _r4, c4) = conn();
        assert_eq!(clients.try_insert(t3, c4).unwrap(), ClientId(3));
        assert_eq!(clients.len(), 2);
    }

    #[test]
    fn pid_and_permission_mode_are_per_client() {
        let clients = Clients::new(4);
        let (t1, _r1, c1) = conn();
        let (t2, _r2, c2) = conn();
        let a = clients.try_insert(t1, c1).unwrap();
        let b = clients.try_insert(t2, c2).unwrap();
        assert!(clients.set_pid(a, 111));
        assert!(clients.set_permission_mode(b, "plan"));
        assert!(!clients.set_pid(ClientId(99), 1));
        let snaps = clients.snapshots();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].pid, Some(111));
        assert_eq!(snaps[0].permission_mode, None);
        assert_eq!(snaps[1].pid, None);
        assert_eq!(snaps[1].permission_mode.as_deref(), Some("plan"));
        assert_eq!(clients.find_by_pid(111), Some(a));
        assert_eq!(clients.find_by_pid(222), None);
        assert_eq!(
            snaps[0].info(),
            ClientInfo {
                id: 1,
                pid: Some(111)
            }
        );
    }

    #[test]
    fn broadcast_counts_live_receivers() {
        let clients = Clients::new(4);
        let (t1, mut r1, c1) = conn();
        let (t2, r2, c2) = conn();
        let a = clients.try_insert(t1, c1).unwrap();
        let _b = clients.try_insert(t2, c2).unwrap();
        drop(r2); // second client's writer is gone
        assert_eq!(clients.broadcast(Message::Text("x".into())), 1);
        assert!(r1.try_recv().is_ok());
        assert!(clients.send_to(a, Message::Text("y".into())));
        assert!(!clients.send_to(ClientId(42), Message::Text("z".into())));
    }
}
