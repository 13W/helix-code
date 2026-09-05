//! Integration test for the Claude Code IDE server embedded in a real
//! `Application` (`--claude-ide`).
//!
//! Run:
//!   cargo test --package helix-term --features integration --test claude_ide_integration

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

/// Drive `future` while pumping the editor event loop, so `McpCommand`s sent
/// by the IDE server are handled by the `Application`.
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

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Responses that arrived while waiting for a different id.
#[derive(Default)]
struct Inbox(std::collections::HashMap<u64, Value>);

/// Wait for the response to an already-sent request, stashing others.
async fn rpc_wait(ws: &mut Ws, inbox: &mut Inbox, id: u64) -> anyhow::Result<Value> {
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

async fn rpc(
    ws: &mut Ws,
    inbox: &mut Inbox,
    id: u64,
    method: &str,
    params: Value,
) -> anyhow::Result<Value> {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await?;
    rpc_wait(ws, inbox, id).await
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_ide_server_lifecycle() -> anyhow::Result<()> {
    // Keep lock files out of the real `~/.claude/ide`.
    let config_dir = tempfile::tempdir()?;
    std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path());

    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), "fn main() {}\nfn helper() {}\n")?;
    let file_path = std::fs::canonicalize(file.path())?;

    let mut app = AppBuilder::new()
        .with_claude_ide(Some("Helix-integration"))
        .with_file(file_path.clone(), None)
        .build()?;

    let session = app
        .editor
        .claude_ide
        .clone()
        .expect("IDE server should have started");
    let lock_path = session.handle.lock_path().to_path_buf();
    assert!(lock_path.starts_with(config_dir.path().join("ide")));
    assert!(lock_path.exists());
    assert!(!session.is_connected());

    let lock: Value = serde_json::from_str(&std::fs::read_to_string(&lock_path)?)?;
    assert_eq!(lock["ideName"], "Helix-integration");
    assert_eq!(lock["transport"], "ws");
    assert_eq!(lock["pid"], std::process::id());
    let workspace = lock["workspaceFolders"][0].as_str().unwrap();
    assert!(std::path::Path::new(workspace).is_absolute());
    let token = lock["authToken"].as_str().unwrap().to_string();

    // Handshake like the CLI does.
    let uri: Uri = format!("ws://127.0.0.1:{}", session.port()).parse()?;
    let request = ClientRequestBuilder::new(uri)
        .with_sub_protocol("mcp")
        .with_header("X-Claude-Code-Ide-Authorization", token);
    let (mut ws, response) = tokio_tungstenite::connect_async(request).await?;
    let mut inbox = Inbox::default();
    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "mcp"
    );
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})
            .to_string()
            .into(),
    ))
    .await?;
    let reply = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await?
        .unwrap()?;
    let reply: Value = serde_json::from_str(reply.to_text()?)?;
    assert_eq!(
        reply["result"]["serverInfo"]["name"],
        "Claude Code Helix MCP"
    );
    for _ in 0..50 {
        if session.is_connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(session.is_connected());

    // getDiagnostics for an open file without diagnostics: exactly one entry,
    // echoing the URI, with the line count and an empty list (PROTO §4.4).
    let uri = format!("file://{}", file_path.display());
    let reply = with_loop(
        &mut app,
        rpc(
            &mut ws,
            &mut inbox,
            1,
            "tools/call",
            json!({"name": "getDiagnostics", "arguments": {"uri": uri}}),
        ),
    )
    .await?;
    assert!(reply.get("error").is_none(), "unexpected error: {reply}");
    assert_ne!(reply["result"]["isError"], true, "tool error: {reply}");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    let files: Value = serde_json::from_str(text)?;
    assert_eq!(files.as_array().map(Vec::len), Some(1));
    assert_eq!(files[0]["uri"], uri);
    assert_eq!(files[0]["linesInFile"], 3);
    assert_eq!(files[0]["diagnostics"], json!([]));

    // Without `uri`: every open document is listed.
    let reply = with_loop(
        &mut app,
        rpc(
            &mut ws,
            &mut inbox,
            2,
            "tools/call",
            json!({"name": "getDiagnostics", "arguments": {}}),
        ),
    )
    .await?;
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    let files: Value = serde_json::from_str(text)?;
    assert!(files.as_array().unwrap().iter().any(|f| f["uri"] == uri));

    // Moving the selection in the editor produces a debounced
    // `selection_changed` notification (PROTO §6.1).
    {
        let (view, doc) = helix_view::current!(app.editor);
        doc.set_selection(view.id, helix_core::Selection::single(0, 12));
    }
    let note = with_loop(&mut app, async {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await?
                .unwrap()?;
            if let Message::Text(text) = msg {
                let value: Value = serde_json::from_str(text.as_str())?;
                if value["method"] == "selection_changed" {
                    return Ok(value);
                }
            }
        }
    })
    .await?;
    assert_eq!(
        note["params"]["filePath"].as_str().unwrap(),
        file_path.to_string_lossy()
    );
    assert_eq!(note["params"]["fileUrl"], uri);
    assert_eq!(note["params"]["text"], "fn main() {}");
    assert_eq!(
        note["params"]["selection"],
        json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 12}, "isEmpty": false})
    );
    assert!(note.get("id").is_none(), "notifications carry no id");

    // openDiff (prompt mode): the proposal blocks until decided; Enter on the
    // first option (Apply) answers FILE_SAVED with the proposed contents and
    // Helix leaves the file alone (the CLI writes it).
    let before = std::fs::read_to_string(&file_path)?;
    let tab = "\u{273B} [Claude Code] test.rs (abc123) \u{29C9}";
    let proposal = "fn main() {}\nfn helper() {}\nfn added() {}\n";
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"openDiff","arguments":{
            "old_file_path": file_path.to_string_lossy(),
            "new_file_path": file_path.to_string_lossy(),
            "new_file_contents": proposal,
            "tab_name": tab,
        }}})
        .to_string()
        .into(),
    ))
    .await?;
    // Let the editor receive the command and show the prompt.
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;
    assert_eq!(session.handler.pending_diff_count(), 1);
    assert!(session.handler.pending_diffs()[0].shown);
    // Nothing answered yet.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), ws.next())
            .await
            .is_err(),
        "openDiff must not resolve before the user decides"
    );
    // Press Enter: the Select's default option is Apply.
    let reply = with_keys_loop(&mut app, "<ret>", rpc_wait(&mut ws, &mut inbox, 10)).await?;
    let content = reply["result"]["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], "FILE_SAVED");
    assert_eq!(content[1]["text"], proposal);
    assert_eq!(
        std::fs::read_to_string(&file_path)?,
        before,
        "Helix must not write the file"
    );
    assert_eq!(session.handler.pending_diff_count(), 0);

    // close_tab on a pending proposal rejects it.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"openDiff","arguments":{
            "old_file_path": file_path.to_string_lossy(),
            "new_file_path": file_path.to_string_lossy(),
            "new_file_contents": "other\n",
            "tab_name": "second",
        }}})
        .to_string()
        .into(),
    ))
    .await?;
    with_loop(&mut app, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await?;
    let closed = with_loop(
        &mut app,
        rpc(
            &mut ws,
            &mut inbox,
            12,
            "tools/call",
            json!({"name": "close_tab", "arguments": {"tab_name": "second"}}),
        ),
    )
    .await?;
    assert_eq!(closed["result"]["content"][0]["text"], "TAB_CLOSED");
    let rejected = with_loop(&mut app, rpc_wait(&mut ws, &mut inbox, 11)).await?;
    assert_eq!(rejected["result"]["content"][0]["text"], "DIFF_REJECTED");
    assert_eq!(rejected["result"]["content"][1]["text"], "second");
    assert_eq!(session.handler.pending_diff_count(), 0);

    // Closing the editor stops the server and removes the lock file.
    let errs = app.close().await;
    assert!(errs.is_empty(), "close() reported errors: {errs:?}");
    assert!(app.editor.claude_ide.is_none());
    assert!(!lock_path.exists(), "lock file must be removed on close");
    assert!(session.handle.is_stopped());
    Ok(())
}
