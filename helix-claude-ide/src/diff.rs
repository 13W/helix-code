//! Registry of pending `openDiff` proposals (PROTO §4.1, §5.3, §5.4).
//!
//! Each proposal is resolved exactly once — by the user (Apply / Reject in
//! the editor), by `close_tab` / `closeAllDiffTabs` from the CLI, or when
//! the client goes away. The editor and the CLI-facing tools share one
//! `oneshot::Sender` behind an `Arc<Mutex<Option<..>>>`: whoever takes it
//! first decides the outcome.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use helix_mcp_types::DiffOutcome;
use tokio::sync::oneshot;

pub type Reply = Arc<Mutex<Option<oneshot::Sender<DiffOutcome>>>>;

#[derive(Debug, Clone)]
pub struct PendingDiff {
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
    entries: Mutex<HashMap<String, Entry>>,
    /// Serialises `openDiff` prompts: only one is shown at a time, the rest
    /// wait here (but are already registered, so `close_tab` can reject them).
    pub show_lock: tokio::sync::Mutex<()>,
}

impl DiffRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a proposal. Returns the shared reply slot (to hand to the
    /// editor) and the receiver the tool call awaits.
    pub fn register(
        &self,
        tab_name: String,
        old_path: PathBuf,
        new_path: PathBuf,
    ) -> (Reply, oneshot::Receiver<DiffOutcome>) {
        let (tx, rx) = oneshot::channel();
        let reply: Reply = Arc::new(Mutex::new(Some(tx)));
        let mut entries = self.entries.lock().unwrap();
        if let Some(previous) = entries.remove(&tab_name) {
            // Same tab name re-used (the CLI closes old diffs first, but be safe).
            resolve_entry(&previous, DiffOutcome::Rejected);
        }
        entries.insert(
            tab_name.clone(),
            Entry {
                info: PendingDiff {
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

    pub fn mark_shown(&self, tab_name: &str) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(tab_name) {
            entry.info.shown = true;
        }
    }

    /// Resolve one proposal from the CLI side. Returns the proposal if it was
    /// still pending (with `shown` telling whether the editor displays it).
    pub fn resolve(&self, tab_name: &str, outcome: DiffOutcome) -> Option<PendingDiff> {
        let entry = self.entries.lock().unwrap().remove(tab_name)?;
        resolve_entry(&entry, outcome);
        Some(entry.info)
    }

    /// Resolve every pending proposal (`closeAllDiffTabs`, disconnect, stop).
    pub fn resolve_all(&self, outcome: DiffOutcome) -> Vec<PendingDiff> {
        let entries: Vec<Entry> = self
            .entries
            .lock()
            .unwrap()
            .drain()
            .map(|(_, e)| e)
            .collect();
        entries
            .into_iter()
            .map(|entry| {
                resolve_entry(&entry, outcome.clone());
                entry.info
            })
            .collect()
    }

    /// Drop the bookkeeping for a proposal that was resolved by the editor.
    pub fn remove(&self, tab_name: &str) {
        self.entries.lock().unwrap().remove(tab_name);
    }

    pub fn is_pending(&self, tab_name: &str) -> bool {
        self.entries.lock().unwrap().contains_key(tab_name)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn pending(&self) -> Vec<PendingDiff> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(|e| e.info.clone())
            .collect()
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

    #[tokio::test]
    async fn resolves_once() {
        let registry = DiffRegistry::new();
        let (reply, rx) = registry.register("t1".into(), "/a".into(), "/a".into());
        assert!(registry.is_pending("t1"));
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
        let info = registry.resolve("t1", DiffOutcome::Rejected).unwrap();
        assert_eq!(info.tab_name, "t1");
        assert!(!registry.is_pending("t1"));
        assert!(matches!(rx.await.unwrap(), DiffOutcome::Saved(s) if s == "x"));
    }

    #[tokio::test]
    async fn resolve_all_rejects_everything() {
        let registry = DiffRegistry::new();
        let (_r1, rx1) = registry.register("a".into(), "/a".into(), "/a".into());
        let (_r2, rx2) = registry.register("b".into(), "/b".into(), "/b".into());
        registry.mark_shown("a");
        let mut resolved = registry.resolve_all(DiffOutcome::Rejected);
        resolved.sort_by(|x, y| x.tab_name.cmp(&y.tab_name));
        assert_eq!(resolved.len(), 2);
        assert!(resolved[0].shown && !resolved[1].shown);
        assert!(registry.is_empty());
        assert!(matches!(rx1.await.unwrap(), DiffOutcome::Rejected));
        assert!(matches!(rx2.await.unwrap(), DiffOutcome::Rejected));
    }

    #[test]
    fn unknown_tab_is_none() {
        let registry = DiffRegistry::new();
        assert!(registry.resolve("nope", DiffOutcome::Rejected).is_none());
    }
}
