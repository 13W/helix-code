//! IDE → CLI notifications (PROTO §6): `selection_changed` with the
//! extension's 300 ms trailing debounce and de-duplication, the 500 ms
//! replay after a client connects (§3.5), and `at_mentioned`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::diagnostics::path_to_uri;

/// Trailing debounce of `selection_changed` (`setTimeout(300)` in the extension).
pub const SELECTION_DEBOUNCE: Duration = Duration::from_millis(300);
/// Delay before the cached selection is replayed to a freshly connected client.
pub const REPLAY_DELAY: Duration = Duration::from_millis(500);

pub const SELECTION_CHANGED: &str = "selection_changed";
pub const AT_MENTIONED: &str = "at_mentioned";

/// 0-indexed line and character (chars, not bytes) — what the CLI expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

/// Primary selection of the focused document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionInfo {
    pub file_path: PathBuf,
    pub text: String,
    pub start: Position,
    pub end: Position,
}

impl SelectionInfo {
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// `params` of a `selection_changed` notification (PROTO §6.1).
    pub fn params(&self) -> Value {
        json!({
            "text": self.text,
            "filePath": self.file_path.to_string_lossy(),
            "fileUrl": path_to_uri(&self.file_path),
            "selection": {
                "start": { "line": self.start.line, "character": self.start.character },
                "end": { "line": self.end.line, "character": self.end.character },
                "isEmpty": self.is_empty(),
            }
        })
    }

    /// Inclusive 0-indexed line span for `at_mentioned`, or `None` for an
    /// empty selection. An end at column 0 does not count that line (the
    /// CLI applies the same rule when counting selected lines).
    pub fn line_span(&self) -> Option<(usize, usize)> {
        if self.is_empty() {
            return None;
        }
        let end_line = if self.end.character == 0 && self.end.line > self.start.line {
            self.end.line - 1
        } else {
            self.end.line
        };
        Some((self.start.line, end_line))
    }
}

/// `params` of an `at_mentioned` notification (PROTO §6.2).
pub fn at_mentioned_params(path: &Path, lines: Option<(usize, usize)>) -> Value {
    let mut params = json!({ "filePath": path.to_string_lossy() });
    if let Some((start, end)) = lines {
        params["lineStart"] = json!(start);
        params["lineEnd"] = json!(end);
    }
    params
}

/// Sends one notification; returns `false` when no client is connected.
pub type SendFn = Arc<dyn Fn(&str, Value) -> bool + Send + Sync>;

enum Event {
    Changed(SelectionInfo),
    Replay(Duration),
}

/// Debounced, de-duplicated `selection_changed` sender.
///
/// `update` is cheap and non-blocking (editor hooks call it on every cursor
/// move); the actual sends happen on a background task.
#[derive(Clone)]
pub struct SelectionTracker {
    tx: mpsc::UnboundedSender<Event>,
    latest: Arc<Mutex<Option<SelectionInfo>>>,
}

impl SelectionTracker {
    /// Spawn the background task on the current Tokio runtime.
    pub fn spawn(send: SendFn) -> Self {
        Self::spawn_with(send, SELECTION_DEBOUNCE)
    }

    pub fn spawn_with(send: SendFn, debounce: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let latest = Arc::new(Mutex::new(None));
        tokio::spawn(run(rx, send, debounce, Arc::clone(&latest)));
        SelectionTracker { tx, latest }
    }

    /// Record a new selection; a frame is sent after the debounce if it
    /// differs from the last one sent.
    pub fn update(&self, info: SelectionInfo) {
        *self.latest.lock().unwrap() = Some(info.clone());
        let _ = self.tx.send(Event::Changed(info));
    }

    /// Re-send the cached selection after `delay` (new client connected).
    pub fn replay_after(&self, delay: Duration) {
        let _ = self.tx.send(Event::Replay(delay));
    }

    pub fn latest(&self) -> Option<SelectionInfo> {
        self.latest.lock().unwrap().clone()
    }
}

async fn run(
    mut rx: mpsc::UnboundedReceiver<Event>,
    send: SendFn,
    debounce: Duration,
    latest: Arc<Mutex<Option<SelectionInfo>>>,
) {
    let mut last_sent: Option<SelectionInfo> = None;
    let mut pending: Option<SelectionInfo> = None;
    let mut deadline: Option<Instant> = None;
    loop {
        let timer = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            event = rx.recv() => match event {
                None => break,
                Some(Event::Changed(info)) => {
                    if last_sent.as_ref() == Some(&info) {
                        // Back to what the client already knows: nothing to send.
                        pending = None;
                        deadline = None;
                    } else {
                        pending = Some(info);
                        deadline = Some(Instant::now() + debounce);
                    }
                }
                Some(Event::Replay(delay)) => {
                    // A new client knows nothing yet; the cache is re-sent as is.
                    last_sent = None;
                    let cached = latest.lock().unwrap().clone();
                    if let Some(info) = cached {
                        pending = Some(info);
                        deadline = Some(Instant::now() + delay);
                    }
                }
            },
            _ = timer => {
                deadline = None;
                if let Some(info) = pending.take() {
                    if send(SELECTION_CHANGED, info.params()) {
                        last_sent = Some(info);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(line: usize, text: &str) -> SelectionInfo {
        SelectionInfo {
            file_path: PathBuf::from("/w/a.rs"),
            text: text.to_string(),
            start: Position { line, character: 0 },
            end: Position {
                line,
                character: text.len(),
            },
        }
    }

    fn capture() -> (SendFn, mpsc::UnboundedReceiver<(String, Value)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let send: SendFn = Arc::new(move |method: &str, params: Value| {
            tx.send((method.to_string(), params)).is_ok()
        });
        (send, rx)
    }

    #[test]
    fn params_shape() {
        let p = info(3, "ab").params();
        assert_eq!(p["filePath"], "/w/a.rs");
        assert_eq!(p["fileUrl"], "file:///w/a.rs");
        assert_eq!(p["text"], "ab");
        assert_eq!(p["selection"]["start"], json!({"line": 3, "character": 0}));
        assert_eq!(p["selection"]["end"], json!({"line": 3, "character": 2}));
        assert_eq!(p["selection"]["isEmpty"], false);
        assert_eq!(info(0, "").params()["selection"]["isEmpty"], true);
    }

    #[test]
    fn line_span_rules() {
        assert_eq!(info(4, "").line_span(), None);
        assert_eq!(info(4, "xy").line_span(), Some((4, 4)));
        let multi = SelectionInfo {
            file_path: PathBuf::from("/w/a.rs"),
            text: "a\nb\n".into(),
            start: Position {
                line: 9,
                character: 0,
            },
            end: Position {
                line: 11,
                character: 0,
            },
        };
        assert_eq!(multi.line_span(), Some((9, 10)));
    }

    #[test]
    fn at_mentioned_shape() {
        assert_eq!(
            at_mentioned_params(Path::new("/w/a.rs"), Some((9, 14))),
            json!({"filePath": "/w/a.rs", "lineStart": 9, "lineEnd": 14})
        );
        assert_eq!(
            at_mentioned_params(Path::new("/w/a.rs"), None),
            json!({"filePath": "/w/a.rs"})
        );
    }

    #[tokio::test(start_paused = true)]
    async fn debounces_and_sends_latest() {
        let (send, mut rx) = capture();
        let tracker = SelectionTracker::spawn_with(send, Duration::from_millis(300));
        tracker.update(info(1, "a"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        tracker.update(info(2, "b"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(250)).await;
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "nothing before the debounce elapses"
        );
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        let (method, params) = rx.try_recv().expect("one frame after 300 ms");
        assert_eq!(method, "selection_changed");
        assert_eq!(params["selection"]["start"]["line"], 2);
        assert!(rx.try_recv().is_err(), "exactly one frame");
    }

    #[tokio::test(start_paused = true)]
    async fn deduplicates_unchanged_selection() {
        let (send, mut rx) = capture();
        let tracker = SelectionTracker::spawn_with(send, Duration::from_millis(300));
        tracker.update(info(1, "a"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok());
        tracker.update(info(1, "a"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "identical selection is not re-sent");
    }

    #[tokio::test(start_paused = true)]
    async fn replays_cache_for_new_client() {
        let (send, mut rx) = capture();
        let tracker = SelectionTracker::spawn_with(send, Duration::from_millis(300));
        tracker.update(info(5, "sel"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok());
        tracker.replay_after(Duration::from_millis(500));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "not before the replay delay");
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        let (_, params) = rx.try_recv().expect("replayed frame");
        assert_eq!(params["text"], "sel");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_to_replay_without_cache() {
        let (send, mut rx) = capture();
        let tracker = SelectionTracker::spawn_with(send, Duration::from_millis(300));
        tracker.replay_after(Duration::from_millis(500));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
    }
}
