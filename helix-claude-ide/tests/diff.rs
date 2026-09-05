//! `openDiff` / `close_tab` / `closeAllDiffTabs` against the editor-backed
//! handler with a fake editor loop standing in for Helix (PROTO §4.1–4.3, §5.3–5.4).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_mcp_types::{DiffOutcome, McpCommand};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use helix_claude_ide::{EditorHandler, ToolHandler};

type Reply = Arc<Mutex<Option<oneshot::Sender<DiffOutcome>>>>;

/// What the fake editor saw.
#[derive(Default)]
struct EditorState {
    /// `tab_name` → reply slot handed over by `OpenDiff`.
    shown: HashMap<String, Reply>,
    /// Order of `OpenDiff` commands.
    opened: Vec<String>,
    /// `CloseDiff` commands.
    closed: Vec<String>,
}

fn fake_editor() -> (Arc<EditorHandler>, Arc<Mutex<EditorState>>) {
    let (tx, mut rx) = mpsc::channel::<McpCommand>(8);
    let state = Arc::new(Mutex::new(EditorState::default()));
    let seen = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                McpCommand::OpenDiff {
                    tab_name, reply, ..
                } => {
                    let mut s = seen.lock().unwrap();
                    s.opened.push(tab_name.clone());
                    s.shown.insert(tab_name, reply);
                }
                McpCommand::CloseDiff { tab_name, reply } => {
                    seen.lock().unwrap().closed.push(tab_name);
                    let _ = reply.send(());
                }
                _ => {}
            }
        }
    });
    (Arc::new(EditorHandler::new(tx)), state)
}

fn open_diff_args(path: &str, tab: &str, contents: &str) -> Value {
    json!({
        "old_file_path": path,
        "new_file_path": path,
        "new_file_contents": contents,
        "tab_name": tab,
    })
}

async fn wait_for<F: Fn() -> bool>(cond: F) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met in time");
}

fn texts(result: &helix_claude_ide::ToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .map(|c| match c {
            helix_claude_ide::Content::Text { text } => text.clone(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn open_diff_blocks_until_the_user_accepts_and_never_writes() {
    let (handler, editor) = fake_editor();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "original\n").unwrap();
    let path = file.path().to_string_lossy().into_owned();
    let mtime = std::fs::metadata(file.path()).unwrap().modified().unwrap();

    let h = Arc::clone(&handler);
    let args = open_diff_args(&path, "✻ [Claude Code] a.rs (abc123) ⧉", "changed\n");
    let call = tokio::spawn(async move { h.call("openDiff", args).await.unwrap() });

    // No answer without a decision.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!call.is_finished(), "openDiff must wait for the user");
    assert_eq!(handler.pending_diff_count(), 1);
    assert!(handler.pending_diffs()[0].shown);

    // The user edits the proposal and accepts: the edited text comes back.
    let reply = editor.lock().unwrap().shown["✻ [Claude Code] a.rs (abc123) ⧉"].clone();
    reply
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(DiffOutcome::Saved("changed by user\n".into()))
        .unwrap();
    let result = call.await.unwrap();
    assert_eq!(texts(&result), ["FILE_SAVED", "changed by user\n"]);
    assert!(!result.is_error);
    assert_eq!(handler.pending_diff_count(), 0);

    // Helix did not touch the file (PROTO §5.2).
    assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "original\n");
    assert_eq!(
        std::fs::metadata(file.path()).unwrap().modified().unwrap(),
        mtime
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_by_user() {
    let (handler, editor) = fake_editor();
    let h = Arc::clone(&handler);
    let call = tokio::spawn(async move {
        h.call("openDiff", open_diff_args("/w/b.rs", "tab-b", "x"))
            .await
            .unwrap()
    });
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-b")).await;
    let reply = editor.lock().unwrap().shown["tab-b"].clone();
    reply
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(DiffOutcome::Rejected)
        .unwrap();
    assert_eq!(texts(&call.await.unwrap()), ["DIFF_REJECTED", "tab-b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_tab_rejects_the_pending_diff_and_dismisses_its_ui() {
    let (handler, editor) = fake_editor();
    let h = Arc::clone(&handler);
    let call = tokio::spawn(async move {
        h.call("openDiff", open_diff_args("/w/c.rs", "tab-c", "x"))
            .await
            .unwrap()
    });
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-c")).await;

    let closed = handler
        .call("close_tab", json!({"tab_name": "tab-c"}))
        .await
        .unwrap();
    assert_eq!(texts(&closed), ["TAB_CLOSED"]);
    assert_eq!(texts(&call.await.unwrap()), ["DIFF_REJECTED", "tab-c"]);
    assert_eq!(editor.lock().unwrap().closed, ["tab-c"]);

    // Unknown tab: still TAB_CLOSED (PROTO §4.2).
    let unknown = handler
        .call("close_tab", json!({"tab_name": "nope"}))
        .await
        .unwrap();
    assert_eq!(texts(&unknown), ["TAB_CLOSED"]);
    assert_eq!(handler.pending_diff_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_all_rejects_shown_and_queued_diffs() {
    let (handler, editor) = fake_editor();
    let h1 = Arc::clone(&handler);
    let first = tokio::spawn(async move {
        h1.call("openDiff", open_diff_args("/w/d.rs", "tab-d1", "1"))
            .await
            .unwrap()
    });
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-d1")).await;
    let h2 = Arc::clone(&handler);
    let second = tokio::spawn(async move {
        h2.call("openDiff", open_diff_args("/w/d.rs", "tab-d2", "2"))
            .await
            .unwrap()
    });
    wait_for(|| handler.pending_diff_count() == 2).await;
    // Only one proposal is on screen; the second waits in the queue.
    assert_eq!(editor.lock().unwrap().opened, ["tab-d1"]);

    let closed = handler.call("closeAllDiffTabs", json!({})).await.unwrap();
    assert_eq!(texts(&closed), ["CLOSED_2_DIFF_TABS"]);
    assert_eq!(texts(&first.await.unwrap()), ["DIFF_REJECTED", "tab-d1"]);
    assert_eq!(texts(&second.await.unwrap()), ["DIFF_REJECTED", "tab-d2"]);
    // The queued one was never shown, so only one UI dismissal.
    {
        let state = editor.lock().unwrap();
        assert_eq!(state.closed, ["tab-d1"]);
        assert_eq!(state.opened, ["tab-d1"]);
    }
    assert_eq!(handler.pending_diff_count(), 0);

    let none = handler.call("closeAllDiffTabs", json!({})).await.unwrap();
    assert_eq!(texts(&none), ["CLOSED_0_DIFF_TABS"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_rejects_everything() {
    let (handler, editor) = fake_editor();
    let h = Arc::clone(&handler);
    let call = tokio::spawn(async move {
        h.call("openDiff", open_diff_args("/w/e.rs", "tab-e", "x"))
            .await
            .unwrap()
    });
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-e")).await;
    handler.on_client_disconnected();
    assert_eq!(texts(&call.await.unwrap()), ["DIFF_REJECTED", "tab-e"]);
    wait_for(|| editor.lock().unwrap().closed == ["tab-e"]).await;
    assert_eq!(handler.pending_diff_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_diff_is_shown_after_the_first_is_decided() {
    let (handler, editor) = fake_editor();
    let h1 = Arc::clone(&handler);
    let first = tokio::spawn(async move {
        h1.call("openDiff", open_diff_args("/w/f.rs", "tab-f1", "1"))
            .await
            .unwrap()
    });
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-f1")).await;
    let h2 = Arc::clone(&handler);
    let second = tokio::spawn(async move {
        h2.call("openDiff", open_diff_args("/w/f.rs", "tab-f2", "2"))
            .await
            .unwrap()
    });
    wait_for(|| handler.pending_diff_count() == 2).await;

    let reply = editor.lock().unwrap().shown["tab-f1"].clone();
    reply
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(DiffOutcome::Saved("1".into()))
        .unwrap();
    assert_eq!(texts(&first.await.unwrap()), ["FILE_SAVED", "1"]);
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-f2")).await;
    assert_eq!(editor.lock().unwrap().opened, ["tab-f1", "tab-f2"]);
    let reply = editor.lock().unwrap().shown["tab-f2"].clone();
    reply
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(DiffOutcome::Rejected)
        .unwrap();
    assert_eq!(texts(&second.await.unwrap()), ["DIFF_REJECTED", "tab-f2"]);
}
