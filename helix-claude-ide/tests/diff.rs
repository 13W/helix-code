//! `openDiff` / `close_tab` / `closeAllDiffTabs` against the editor-backed
//! handler with a fake editor loop standing in for Helix (PROTO §4.1–4.3,
//! §5.3–5.4), including the per-client isolation of T8.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_mcp_types::{DiffOutcome, McpCommand};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use helix_claude_ide::{ClientId, EditorHandler, ToolHandler};

type Reply = Arc<Mutex<Option<oneshot::Sender<DiffOutcome>>>>;

const A: ClientId = ClientId(1);
const B: ClientId = ClientId(2);

/// What the fake editor saw.
#[derive(Default)]
struct EditorState {
    /// `tab_name` → reply slot handed over by `OpenDiff`.
    shown: HashMap<String, Reply>,
    /// Order of `OpenDiff` commands: (client id, tab name).
    opened: Vec<(u64, String)>,
    /// `CloseDiff` commands: (client id, tab name).
    closed: Vec<(u64, String)>,
}

fn fake_editor() -> (Arc<EditorHandler>, Arc<Mutex<EditorState>>) {
    let (tx, mut rx) = mpsc::channel::<McpCommand>(8);
    let state = Arc::new(Mutex::new(EditorState::default()));
    let seen = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                McpCommand::OpenDiff {
                    client,
                    tab_name,
                    reply,
                    ..
                } => {
                    let mut s = seen.lock().unwrap();
                    s.opened.push((client.id, tab_name.clone()));
                    s.shown.insert(tab_name, reply);
                }
                McpCommand::CloseDiff {
                    client,
                    tab_name,
                    reply,
                } => {
                    seen.lock().unwrap().closed.push((client.id, tab_name));
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

fn decide(editor: &Arc<Mutex<EditorState>>, tab: &str, outcome: DiffOutcome) {
    let reply = editor.lock().unwrap().shown[tab].clone();
    reply.lock().unwrap().take().unwrap().send(outcome).unwrap();
}

fn spawn_open(
    handler: &Arc<EditorHandler>,
    client: ClientId,
    path: &str,
    tab: &str,
    contents: &str,
) -> tokio::task::JoinHandle<helix_claude_ide::ToolResult> {
    let h = Arc::clone(handler);
    let args = open_diff_args(path, tab, contents);
    tokio::spawn(async move { h.call(client, "openDiff", args).await.unwrap() })
}

#[tokio::test(flavor = "multi_thread")]
async fn open_diff_blocks_until_the_user_accepts_and_never_writes() {
    let (handler, editor) = fake_editor();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "original\n").unwrap();
    let path = file.path().to_string_lossy().into_owned();
    let mtime = std::fs::metadata(file.path()).unwrap().modified().unwrap();

    let tab = "✻ [Claude Code] a.rs (abc123) ⧉";
    let call = spawn_open(&handler, A, &path, tab, "changed\n");

    // No answer without a decision.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!call.is_finished(), "openDiff must wait for the user");
    assert_eq!(handler.pending_diff_count(), 1);
    assert_eq!(handler.pending_count_for(A), 1);
    assert_eq!(handler.pending_count_for(B), 0);
    let pending = &handler.pending_diffs()[0];
    assert!(pending.shown);
    assert_eq!(pending.client, A);
    assert_eq!(handler.last_shown_client(), Some(A));
    assert_eq!(editor.lock().unwrap().opened, [(1, tab.to_string())]);

    // The user edits the proposal and accepts: the edited text comes back.
    decide(&editor, tab, DiffOutcome::Saved("changed by user\n".into()));
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
    let call = spawn_open(&handler, A, "/w/b.rs", "tab-b", "x");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-b")).await;
    decide(&editor, "tab-b", DiffOutcome::Rejected);
    assert_eq!(texts(&call.await.unwrap()), ["DIFF_REJECTED", "tab-b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_tab_rejects_the_pending_diff_and_dismisses_its_ui() {
    let (handler, editor) = fake_editor();
    let call = spawn_open(&handler, A, "/w/c.rs", "tab-c", "x");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-c")).await;

    let closed = handler
        .call(A, "close_tab", json!({"tab_name": "tab-c"}))
        .await
        .unwrap();
    assert_eq!(texts(&closed), ["TAB_CLOSED"]);
    assert_eq!(texts(&call.await.unwrap()), ["DIFF_REJECTED", "tab-c"]);
    assert_eq!(editor.lock().unwrap().closed, [(1, "tab-c".to_string())]);

    // Unknown tab: still TAB_CLOSED (PROTO §4.2).
    let unknown = handler
        .call(A, "close_tab", json!({"tab_name": "nope"}))
        .await
        .unwrap();
    assert_eq!(texts(&unknown), ["TAB_CLOSED"]);
    assert_eq!(handler.pending_diff_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_all_rejects_shown_and_queued_diffs() {
    let (handler, editor) = fake_editor();
    let first = spawn_open(&handler, A, "/w/d.rs", "tab-d1", "1");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-d1")).await;
    let second = spawn_open(&handler, A, "/w/d.rs", "tab-d2", "2");
    wait_for(|| handler.pending_diff_count() == 2).await;
    // Only one proposal is on screen; the second waits in the queue.
    assert_eq!(editor.lock().unwrap().opened, [(1, "tab-d1".to_string())]);

    let closed = handler.call(A, "closeAllDiffTabs", json!({})).await.unwrap();
    assert_eq!(texts(&closed), ["CLOSED_2_DIFF_TABS"]);
    assert_eq!(texts(&first.await.unwrap()), ["DIFF_REJECTED", "tab-d1"]);
    assert_eq!(texts(&second.await.unwrap()), ["DIFF_REJECTED", "tab-d2"]);
    // The queued one was never shown, so only one UI dismissal.
    {
        let state = editor.lock().unwrap();
        assert_eq!(state.closed, [(1, "tab-d1".to_string())]);
        assert_eq!(state.opened, [(1, "tab-d1".to_string())]);
    }
    assert_eq!(handler.pending_diff_count(), 0);

    let none = handler.call(A, "closeAllDiffTabs", json!({})).await.unwrap();
    assert_eq!(texts(&none), ["CLOSED_0_DIFF_TABS"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_rejects_everything() {
    let (handler, editor) = fake_editor();
    let call = spawn_open(&handler, A, "/w/e.rs", "tab-e", "x");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-e")).await;
    handler.on_client_disconnected(A);
    assert_eq!(texts(&call.await.unwrap()), ["DIFF_REJECTED", "tab-e"]);
    wait_for(|| editor.lock().unwrap().closed == [(1, "tab-e".to_string())]).await;
    assert_eq!(handler.pending_diff_count(), 0);
    assert_eq!(handler.last_shown_client(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_diff_is_shown_after_the_first_is_decided() {
    let (handler, editor) = fake_editor();
    let first = spawn_open(&handler, A, "/w/f.rs", "tab-f1", "1");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-f1")).await;
    let second = spawn_open(&handler, A, "/w/f.rs", "tab-f2", "2");
    wait_for(|| handler.pending_diff_count() == 2).await;

    decide(&editor, "tab-f1", DiffOutcome::Saved("1".into()));
    assert_eq!(texts(&first.await.unwrap()), ["FILE_SAVED", "1"]);
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-f2")).await;
    assert_eq!(
        editor.lock().unwrap().opened,
        [(1, "tab-f1".to_string()), (1, "tab-f2".to_string())]
    );
    decide(&editor, "tab-f2", DiffOutcome::Rejected);
    assert_eq!(texts(&second.await.unwrap()), ["DIFF_REJECTED", "tab-f2"]);
}

// ── T8: per-client isolation ─────────────────────────────────────────────────

/// `closeAllDiffTabs` from A (sent at the start of every CLI turn) must not
/// reject what B is waiting for.
#[tokio::test(flavor = "multi_thread")]
async fn close_all_is_client_scoped() {
    let (handler, editor) = fake_editor();
    let a = spawn_open(&handler, A, "/w/g.rs", "tab-a", "a");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-a")).await;
    let b = spawn_open(&handler, B, "/w/h.rs", "tab-b", "b");
    wait_for(|| handler.pending_diff_count() == 2).await;
    assert_eq!(handler.pending_count_for(A), 1);
    assert_eq!(handler.pending_count_for(B), 1);

    let closed = handler.call(A, "closeAllDiffTabs", json!({})).await.unwrap();
    assert_eq!(texts(&closed), ["CLOSED_1_DIFF_TABS"]);
    assert_eq!(texts(&a.await.unwrap()), ["DIFF_REJECTED", "tab-a"]);
    // B's proposal moves to the screen and stays pending.
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-b")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!b.is_finished(), "B's proposal must survive A's closeAllDiffTabs");
    assert_eq!(handler.pending_count_for(B), 1);
    assert_eq!(handler.last_shown_client(), Some(B));
    assert_eq!(editor.lock().unwrap().closed, [(1, "tab-a".to_string())]);

    let closed = handler.call(B, "closeAllDiffTabs", json!({})).await.unwrap();
    assert_eq!(texts(&closed), ["CLOSED_1_DIFF_TABS"]);
    assert_eq!(texts(&b.await.unwrap()), ["DIFF_REJECTED", "tab-b"]);
    assert_eq!(
        editor.lock().unwrap().closed,
        [(1, "tab-a".to_string()), (2, "tab-b".to_string())]
    );
    assert_eq!(handler.pending_diff_count(), 0);
}

/// `close_tab` only knows the caller's own tab names.
#[tokio::test(flavor = "multi_thread")]
async fn close_tab_foreign_name_is_noop() {
    let (handler, editor) = fake_editor();
    let b = spawn_open(&handler, B, "/w/i.rs", "shared-name", "b");
    wait_for(|| editor.lock().unwrap().shown.contains_key("shared-name")).await;

    let closed = handler
        .call(A, "close_tab", json!({"tab_name": "shared-name"}))
        .await
        .unwrap();
    assert_eq!(texts(&closed), ["TAB_CLOSED"]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!b.is_finished(), "A cannot close B's proposal");
    assert!(editor.lock().unwrap().closed.is_empty());

    let closed = handler
        .call(B, "close_tab", json!({"tab_name": "shared-name"}))
        .await
        .unwrap();
    assert_eq!(texts(&closed), ["TAB_CLOSED"]);
    assert_eq!(texts(&b.await.unwrap()), ["DIFF_REJECTED", "shared-name"]);
    assert_eq!(
        editor.lock().unwrap().closed,
        [(2, "shared-name".to_string())]
    );
}

/// A client going away rejects only its own proposals; the other client's
/// queued proposal is then shown.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_rejects_only_own() {
    let (handler, editor) = fake_editor();
    let a = spawn_open(&handler, A, "/w/j.rs", "tab-a", "a");
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-a")).await;
    let b = spawn_open(&handler, B, "/w/j.rs", "tab-b", "b");
    wait_for(|| handler.pending_diff_count() == 2).await;

    handler.on_client_disconnected(A);
    assert_eq!(texts(&a.await.unwrap()), ["DIFF_REJECTED", "tab-a"]);
    wait_for(|| editor.lock().unwrap().closed == [(1, "tab-a".to_string())]).await;
    wait_for(|| editor.lock().unwrap().shown.contains_key("tab-b")).await;
    assert!(!b.is_finished());
    assert_eq!(handler.pending_count_for(B), 1);

    decide(&editor, "tab-b", DiffOutcome::Saved("b!".into()));
    assert_eq!(texts(&b.await.unwrap()), ["FILE_SAVED", "b!"]);
    assert_eq!(handler.pending_diff_count(), 0);
}
