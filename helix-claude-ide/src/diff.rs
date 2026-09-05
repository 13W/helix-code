//! Registry of pending `openDiff` proposals (PROTO §4.1, §5.3, §5.4).
//!
//! Each proposal is resolved exactly once — by the user (Apply / Reject in
//! the editor), by `close_tab` / `closeAllDiffTabs` from the CLI, or when
//! the client goes away. The editor and the CLI-facing tools share one
//! `oneshot::Sender` behind an `Arc<Mutex<Option<..>>>`: whoever takes it
//! first decides the outcome.
//!
//! Proposals are keyed by `(ClientId, tab_name)` (T8): `tab_name` is only
//! unique per CLI, and `closeAllDiffTabs` — which every CLI sends at the
//! start of each turn — must only touch that CLI's own proposals.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use helix_mcp_types::DiffOutcome;
use tokio::sync::oneshot;

use crate::clients::ClientId;

pub type Reply = Arc<Mutex<Option<oneshot::Sender<DiffOutcome>>>>;

type Key = (ClientId, String);

#[derive(Debug, Clone)]
pub struct PendingDiff {
    /// The CLI connection that proposed it.
    pub client: ClientId,
    pub tab_name: String,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    /// Whether the proposal is currently displayed by the editor (at most one is).
    pub shown: bool,
}

struct Entry {
    info: PendingDiff,
    reply: Reply,
}

#[derive(Default)]
pub struct DiffRegistry {
    entries: Mutex<HashMap<Key, Entry>>,
    /// Serialises `openDiff` prompts: only one is shown at a time, the rest
    /// wait here (but are already registered, so `close_tab` can reject them).
    /// Shared by all clients — there is one screen.
    pub show_lock: tokio::sync::Mutex<()>,
    /// Client whose proposal was displayed most recently; the default target
    /// for `:claude-mention` when no focus is set (T8 §2).
    last_shown_client: Mutex<Option<ClientId>>,
}

impl DiffRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a proposal. Returns the shared reply slot (to hand to the
    /// editor) and the receiver the tool call awaits.
    pub fn register(
        &self,
        client: ClientId,
        tab_name: String,
        old_path: PathBuf,
        new_path: PathBuf,
    ) -> (Reply, oneshot::Receiver<DiffOutcome>) {
        let (tx, rx) = oneshot::channel();
        let reply: Reply = Arc::new(Mutex::new(Some(tx)));
        let mut entries = self.entries.lock().unwrap();
        let key = (client, tab_name.clone());
        if let Some(previous) = entries.remove(&key) {
            // Same tab name re-used by the same CLI (it closes old diffs first, but be safe).
            resolve_entry(&previous, DiffOutcome::Rejected);
        }
        entries.insert(
            key,
            Entry {
                info: PendingDiff {
                    client,
                    tab_name,
                    old_path,
                    new_path,
                    shown: false,
                },
                reply: Arc::clone(&reply),
            },
        );
        (reply, rx)
    }

    pub fn mark_shown(&self, client: ClientId, tab_name: &str) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap()
            .get_mut(&(client, tab_name.to_string()))
        {
            entry.info.shown = true;
        }
        *self.last_shown_client.lock().unwrap() = Some(client);
    }

    /// Resolve one of `client`'s proposals from the CLI side. Returns it if it
    /// was still pending (with `shown` telling whether the editor displays it).
    pub fn resolve(
        &self,
        client: ClientId,
        tab_name: &str,
        outcome: DiffOutcome,
    ) -> Option<PendingDiff> {
        let entry = self
            .entries
            .lock()
            .unwrap()
            .remove(&(client, tab_name.to_string()))?;
        resolve_entry(&entry, outcome);
        Some(entry.info)
    }

    /// Resolve every pending proposal of `client` (`closeAllDiffTabs`, disconnect).
    pub fn resolve_all_for(&self, client: ClientId, outcome: DiffOutcome) -> Vec<PendingDiff> {
        let entries: Vec<Entry> = {
            let mut map = self.entries.lock().unwrap();
            let keys: Vec<Key> = map.keys().filter(|(c, _)| *c == client).cloned().collect();
            keys.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        Self::finish(entries, outcome)
    }

    /// Resolve every pending proposal of every client (server stop).
    pub fn resolve_all(&self, outcome: DiffOutcome) -> Vec<PendingDiff> {
        let entries: Vec<Entry> = self
            .entries
            .lock()
            .unwrap()
            .drain()
            .map(|(_, e)| e)
            .collect();
        Self::finish(entries, outcome)
    }

    fn finish(entries: Vec<Entry>, outcome: DiffOutcome) -> Vec<PendingDiff> {
        let mut resolved: Vec<PendingDiff> = entries
            .into_iter()
            .map(|entry| {
                resolve_entry(&entry, outcome.clone());
                entry.info
            })
            .collect();
        resolved.sort_by(|a, b| (a.client, &a.tab_name).cmp(&(b.client, &b.tab_name)));
        resolved
    }

    /// Drop the bookkeeping for a proposal that was resolved by the editor.
    pub fn remove(&self, client: ClientId, tab_name: &str) {
        self.entries
            .lock()
            .unwrap()
            .remove(&(client, tab_name.to_string()));
    }

    pub fn is_pending(&self, client: ClientId, tab_name: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .contains_key(&(client, tab_name.to_string()))
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn count_for(&self, client: ClientId) -> usize {
        self.entries
            .lock()
            .unwrap()
            .keys()
            .filter(|(c, _)| *c == client)
            .count()
    }

    /// All pending proposals, ordered by client then tab name.
    pub fn pending(&self) -> Vec<PendingDiff> {
        let mut all: Vec<PendingDiff> = self
            .entries
            .lock()
            .unwrap()
            .values()
            .map(|e| e.info.clone())
            .collect();
        all.sort_by(|a, b| (a.client, &a.tab_name).cmp(&(b.client, &b.tab_name)));
        all
    }

    pub fn pending_for(&self, client: ClientId) -> Vec<PendingDiff> {
        self.pending()
            .into_iter()
            .filter(|p| p.client == client)
            .collect()
    }

    pub fn last_shown_client(&self) -> Option<ClientId> {
        *self.last_shown_client.lock().unwrap()
    }

    /// Forget `client` as the last shown one (it disconnected).
    pub fn forget_client(&self, client: ClientId) {
        let mut last = self.last_shown_client.lock().unwrap();
        if *last == Some(client) {
            *last = None;
        }
    }
}

fn resolve_entry(entry: &Entry, outcome: DiffOutcome) {
    if let Some(tx) = entry.reply.lock().unwrap().take() {
        let _ = tx.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ClientId = ClientId(1);
    const B: ClientId = ClientId(2);

    #[tokio::test]
    async fn resolves_once() {
        let registry = DiffRegistry::new();
        let (reply, rx) = registry.register(A, "t1".into(), "/a".into(), "/a".into());
        assert!(registry.is_pending(A, "t1"));
        assert!(!registry.is_pending(B, "t1"));
        assert_eq!(registry.len(), 1);
        // The editor answers first.
        reply
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(DiffOutcome::Saved("x".into()))
            .unwrap();
        // A later close_tab finds the sender gone but still clears the entry.
        let info = registry.resolve(A, "t1", DiffOutcome::Rejected).unwrap();
        assert_eq!(info.tab_name, "t1");
        assert_eq!(info.client, A);
        assert!(!registry.is_pending(A, "t1"));
        assert!(matches!(rx.await.unwrap(), DiffOutcome::Saved(s) if s == "x"));
    }

    #[tokio::test]
    async fn resolve_all_for_is_client_scoped() {
        let registry = DiffRegistry::new();
        let (_r1, rx1) = registry.register(A, "a".into(), "/a".into(), "/a".into());
        let (_r2, rx2) = registry.register(A, "b".into(), "/b".into(), "/b".into());
        let (_r3, mut rx3) = registry.register(B, "a".into(), "/a".into(), "/a".into());
        registry.mark_shown(A, "a");
        assert_eq!(registry.last_shown_client(), Some(A));
        assert_eq!(registry.count_for(A), 2);
        assert_eq!(registry.count_for(B), 1);

        let resolved = registry.resolve_all_for(A, DiffOutcome::Rejected);
        assert_eq!(resolved.len(), 2);
        assert!(resolved[0].shown && !resolved[1].shown);
        assert_eq!(registry.len(), 1);
        assert!(registry.is_pending(B, "a"));
        assert!(matches!(rx1.await.unwrap(), DiffOutcome::Rejected));
        assert!(matches!(rx2.await.unwrap(), DiffOutcome::Rejected));
        assert!(rx3.try_recv().is_err(), "B's proposal is untouched");

        // Same tab name, other client: a no-op for B's entry.
        assert!(registry.resolve(A, "a", DiffOutcome::Rejected).is_none());
        assert!(registry.is_pending(B, "a"));

        registry.forget_client(A);
        assert_eq!(registry.last_shown_client(), None);
        registry.mark_shown(B, "a");
        assert_eq!(registry.last_shown_client(), Some(B));
        registry.forget_client(A);
        assert_eq!(registry.last_shown_client(), Some(B));

        let all = registry.resolve_all(DiffOutcome::Rejected);
        assert_eq!(all.len(), 1);
        assert!(registry.is_empty());
        assert!(matches!(rx3.await.unwrap(), DiffOutcome::Rejected));
    }

    #[test]
    fn unknown_tab_is_none() {
        let registry = DiffRegistry::new();
        assert!(registry.resolve(A, "nope", DiffOutcome::Rejected).is_none());
        assert!(registry.pending_for(A).is_empty());
    }
}
