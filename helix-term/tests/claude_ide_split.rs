//! `diff-mode = "split"`: a Claude Code proposal opens as a vertical split.
//!
//! Separate test binary because only the first `Application` in a process
//! receives `McpCommand`s (the channel sender lives in a `OnceLock`).
//!
//! Run:
//!   cargo test --package helix-term --features integration --test claude_ide_split

#![cfg(feature = "integration")]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;

mod test {
    pub mod helpers;
}
use test::helpers::{run_event_loop_until_idle, AppBuilder};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn with_loop<T>(
    app: &mut helix_term::application::Application,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        result = future => result,
        _ = async {
            loop {
                run_event_loop_until_idle(app).await;
                tokio::task::yield_now().await;
            }
        } => unreachable!(),
    }
}

/// Responses that arrived while waiting for another id.
#[derive(Default)]
struct Inbox(std::collections::HashMap<u64, Value>);

async fn wait_response(ws: &mut Ws, inbox: &mut Inbox, id: u64) -> anyhow::Result<Value> {
    if let Some(value) = inbox.0.remove(&id) {
        return Ok(value);
    }
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await?
            .unwrap()?;
        if let Message::Text(text) = msg {
            let value: Value = serde_json::from_str(text.as_str())?;
            match value.get("id").and_then(Value::as_u64) {
                Some(got) if got == id => return Ok(value),
                Some(got) => {
                    inbox.0.insert(got, value);
                }
                None => {}
            }
        }
    }
}

async fn send(ws: &mut Ws, id: u64, method: &str, params: Value) -> anyhow::Result<()> {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proposal_opens_as_split_and_close_tab_tears_it_down() -> anyhow::Result<()> {
    let config_dir = tempfile::tempdir()?;
    std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path());

    let file = tempfile::Builder::new().suffix(".rs").tempfile()?;
    std::fs::write(file.path(), "fn main() {}\n")?;
    let file_path = std::fs::canonicalize(file.path())?;

    let mut config = helix_term::config::Config::default();
    config.editor.claude_ide.diff_mode = helix_view::editor::ClaudeIdeDiffMode::Split;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_claude_ide(Some("Helix-split"))
        .with_file(file_path.clone(), None)
        .build()?;
    let session = app.editor.claude_ide.clone().expect("IDE server started");
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(session.handle.lock_path())?)?;
    let uri: Uri = format!("ws://127.0.0.1:{}", session.port()).parse()?;
    let request = ClientRequestBuilder::new(uri)
        .with_sub_protocol("mcp")
        .with_header(
            "X-Claude-Code-Ide-Authorization",
            lock["authToken"].as_str().unwrap().to_string(),
        );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
    let mut inbox = Inbox::default();
    send(
        &mut ws,
        0,
        "initialize",
        json!({"protocolVersion": "2025-11-25"}),
    )
    .await?;
    wait_response(&mut ws, &mut inbox, 0).await?;

    let views_before = app.editor.tree.views().count();
    assert_eq!(views_before, 1);
    let tab = "\u{273B} [Claude Code] split.rs (0001) \u{29C9}";
    let proposal = "fn main() {}\nfn extra() {}\n";
    send(
        &mut ws,
        1,
        "tools/call",
        json!({"name": "openDiff", "arguments": {
            "old_file_path": file_path.to_string_lossy(),
            "new_file_path": file_path.to_string_lossy(),
            "new_file_contents": proposal,
            "tab_name": tab,
        }}),
    )
    .await?;
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;

    // Split is open: the original window plus left/right views.
    assert_eq!(app.editor.claude_diff_views.len(), 1);
    let view = app.editor.claude_diff_views[0].clone();
    assert_eq!(view.tab_name, tab);
    assert_eq!(view.path, file_path);
    assert!(!view.left_is_scratch);
    assert_eq!(app.editor.tree.views().count(), 3);
    let left = app.editor.documents.get(&view.left).unwrap();
    assert_eq!(left.path(), Some(file_path.as_path()));
    assert!(
        left.readonly,
        "left side is read-only while the proposal is open"
    );
    assert!(
        left.diff_handle().is_some(),
        "left side shows what the proposal changes"
    );
    let right = app.editor.documents.get(&view.right).unwrap();
    assert_eq!(right.text().to_string(), proposal);
    assert_ne!(
        right.path(),
        Some(file_path.as_path()),
        "proposal must not alias the real file"
    );
    // `✻ <file> [<pid or #N>]`: no pid was announced by this fake client.
    assert_eq!(
        right.path().unwrap().file_name().unwrap().to_string_lossy(),
        format!(
            "\u{273B} {} [#1]",
            file_path.file_name().unwrap().to_string_lossy()
        )
    );
    assert_eq!(view.client.id, 1);
    assert_eq!(view.client.pid, None);
    assert_eq!(
        right.language_name(),
        Some("rust"),
        "proposal buffer picks up the language of the target file"
    );
    assert!(
        right.diff_handle().is_some(),
        "right side shows added lines"
    );
    assert!(!right.is_modified(), "unedited proposal counts as clean");
    assert_eq!(session.handler.pending_diff_count(), 1);
    // Focus ended on the right (proposal) view.
    assert_eq!(app.editor.tree.get(app.editor.tree.focus).doc, view.right);
    // Both cursors sit on the first change (the added line 2).
    let right_line = right
        .selection(view.views[1])
        .primary()
        .cursor_line(right.text().slice(..));
    assert_eq!(right_line, 1, "right cursor on the added line");
    let left_line = left
        .selection(view.views[0])
        .primary()
        .cursor_line(left.text().slice(..));
    assert_eq!(left_line, 1, "left cursor where the insertion goes");
    // The proposal buffer is never reported to the CLI as a file.
    let leaked = with_loop(&mut app, async {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: Value = serde_json::from_str(text.as_str())?;
                    if value["method"] == "selection_changed"
                        && value["params"]["filePath"]
                            .as_str()
                            .is_some_and(|p| p.contains('\u{273B}'))
                    {
                        return Ok(Some(value));
                    }
                }
                Ok(Some(Ok(_))) => {}
                _ => return Ok(None),
            }
        }
    })
    .await?;
    assert!(leaked.is_none(), "proposal buffer leaked as selection_changed: {leaked:?}");

    // Closing a *window* (:q) leaves the proposal pending.
    app.editor.close(app.editor.tree.focus);
    assert_eq!(app.editor.claude_diff_views.len(), 1);
    assert_eq!(session.handler.pending_diff_count(), 1);

    // close_tab from the CLI rejects and tears everything down.
    send(
        &mut ws,
        2,
        "tools/call",
        json!({"name": "close_tab", "arguments": {"tab_name": tab}}),
    )
    .await?;
    let closed = with_loop(&mut app, wait_response(&mut ws, &mut inbox, 2)).await?;
    assert_eq!(closed["result"]["content"][0]["text"], "TAB_CLOSED");
    let rejected = with_loop(&mut app, wait_response(&mut ws, &mut inbox, 1)).await?;
    assert_eq!(rejected["result"]["content"][0]["text"], "DIFF_REJECTED");
    assert!(app.editor.claude_diff_views.is_empty());
    assert!(!app.editor.documents.contains_key(&view.right));
    let left = app.editor.documents.get(&view.left).unwrap();
    assert!(!left.readonly);
    assert!(
        left.diff_handle().is_none(),
        "left diff base restored (none before)"
    );
    assert_eq!(app.editor.tree.views().count(), 1);
    assert_eq!(std::fs::read_to_string(&file_path)?, "fn main() {}\n");

    // A proposal for a file that does not exist yet: left side is a scratch buffer.
    let new_file = file_path.with_file_name("brand_new.rs");
    send(
        &mut ws,
        3,
        "tools/call",
        json!({"name": "openDiff", "arguments": {
            "old_file_path": new_file.to_string_lossy(),
            "new_file_path": new_file.to_string_lossy(),
            "new_file_contents": "pub fn new_thing() {}\n",
            "tab_name": "new-file",
        }}),
    )
    .await?;
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;
    let view = app.editor.claude_diff_views[0].clone();
    assert!(view.left_is_scratch);
    assert!(app
        .editor
        .documents
        .get(&view.left)
        .unwrap()
        .path()
        .is_none());
    send(
        &mut ws,
        4,
        "tools/call",
        json!({"name": "closeAllDiffTabs", "arguments": {}}),
    )
    .await?;
    let closed = with_loop(&mut app, wait_response(&mut ws, &mut inbox, 4)).await?;
    assert_eq!(closed["result"]["content"][0]["text"], "CLOSED_1_DIFF_TABS");
    with_loop(&mut app, wait_response(&mut ws, &mut inbox, 3)).await?;
    assert!(app.editor.claude_diff_views.is_empty());
    assert!(!app.editor.documents.contains_key(&view.left));
    assert!(!app.editor.documents.contains_key(&view.right));
    assert!(!new_file.exists());

    // ── T6b: decisions from the editor ────────────────────────────────────

    let open = |id: u64, tab: &str, contents: &str| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"openDiff","arguments":{
            "old_file_path": file_path.to_string_lossy(),
            "new_file_path": file_path.to_string_lossy(),
            "new_file_contents": contents,
            "tab_name": tab,
        }}})
        .to_string()
    };
    let disk_before = std::fs::read_to_string(&file_path)?;

    // (a) :claude-diff-accept with an edited proposal → FILE_SAVED + edited text.
    ws.send(Message::Text(
        open(10, "accept-me", "fn main() {}\nfn a() {}\n").into(),
    ))
    .await?;
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;
    assert_eq!(app.editor.claude_diff_views.len(), 1);
    {
        use helix_core::{Selection, Transaction};
        let (view, doc) = helix_view::current!(app.editor);
        let end = doc.text().len_chars();
        let tx = Transaction::insert(doc.text(), &Selection::point(end), "// edited\n".into());
        doc.apply(&tx, view.id);
        assert!(doc.is_modified());
    }
    let reply = with_keys_loop(
        &mut app,
        ":claude-diff-accept<ret>",
        wait_response(&mut ws, &mut inbox, 10),
    )
    .await?;
    assert_eq!(reply["result"]["content"][0]["text"], "FILE_SAVED");
    assert_eq!(
        reply["result"]["content"][1]["text"],
        "fn main() {}\nfn a() {}\n// edited\n"
    );
    assert!(app.editor.claude_diff_views.is_empty());
    assert_eq!(app.editor.tree.views().count(), 1);
    assert_eq!(
        std::fs::read_to_string(&file_path)?,
        disk_before,
        "Helix never writes"
    );

    // (b) :w in the proposal buffer → FILE_SAVED with the proposal as is.
    ws.send(Message::Text(
        open(11, "write-me", "fn main() {}\nfn b() {}\n").into(),
    ))
    .await?;
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;
    let right = app.editor.claude_diff_views[0].right;
    let reply = with_keys_loop(&mut app, ":w<ret>", wait_response(&mut ws, &mut inbox, 11)).await?;
    assert_eq!(reply["result"]["content"][0]["text"], "FILE_SAVED");
    assert_eq!(
        reply["result"]["content"][1]["text"],
        "fn main() {}\nfn b() {}\n"
    );
    assert!(app.editor.claude_diff_views.is_empty());
    assert!(!app.editor.documents.contains_key(&right));
    assert!(
        !file_path
            .with_file_name(format!(
                "\u{273B} {} [#1]",
                file_path.file_name().unwrap().to_str().unwrap()
            ))
            .exists(),
        ":w must not create the proposal file"
    );
    assert_eq!(std::fs::read_to_string(&file_path)?, disk_before);

    // (c) :bc on the proposal buffer → DIFF_REJECTED.
    ws.send(Message::Text(
        open(12, "close-me", "fn main() {}\nfn c() {}\n").into(),
    ))
    .await?;
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;
    let reply =
        with_keys_loop(&mut app, ":bc<ret>", wait_response(&mut ws, &mut inbox, 12)).await?;
    assert_eq!(reply["result"]["content"][0]["text"], "DIFF_REJECTED");
    assert_eq!(reply["result"]["content"][1]["text"], "close-me");
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    })
    .await?;
    assert!(app.editor.claude_diff_views.is_empty());
    assert_eq!(app.editor.tree.views().count(), 1);
    assert_eq!(std::fs::read_to_string(&file_path)?, disk_before);
    assert_eq!(session.handler.pending_diff_count(), 0);

    app.close().await;
    Ok(())
}

/// Like `with_loop`, but first feeds `keys` (macro syntax) to the editor.
async fn with_keys_loop<T>(
    app: &mut helix_term::application::Application,
    keys: &str,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    #[cfg(windows)]
    use crossterm::event::{Event, KeyEvent};
    use helix_view::input::parse_macro;
    #[cfg(not(windows))]
    use termina::event::{Event, KeyEvent};
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    for key in parse_macro(keys)? {
        tx.send(Ok(Event::Key(KeyEvent::from(key))))?;
    }
    tokio::select! {
        result = future => result,
        _ = async {
            loop {
                app.event_loop_until_idle(&mut rx_stream).await;
                tokio::task::yield_now().await;
            }
        } => unreachable!(),
    }
}
