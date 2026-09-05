//! T8: several `claude` CLIs connected to one Helix (`diff-mode = "split"`):
//! focus / `:claude-mention` addressing and the picker, two simultaneous
//! split proposals from two clients, per-client `closeAllDiffTabs`,
//! `:claude-ide-disconnect`.
//!
//! Separate test binary because only the first `Application` in a process
//! receives `McpCommand`s (the channel sender lives in a `OnceLock`).
//!
//! Run:
//!   cargo test --package helix-term --features integration --test claude_ide_multi

#![cfg(feature = "integration")]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;

use helix_claude_ide::ClientId;

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

/// Feed `keys` and pump the event loop long enough for them (and the jobs
/// they schedule, e.g. a picker) to be processed.
async fn keys(app: &mut helix_term::application::Application, keys: &str) -> anyhow::Result<()> {
    with_keys_loop(app, keys, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    })
    .await
}

async fn settle(app: &mut helix_term::application::Application, ms: u64) -> anyhow::Result<()> {
    with_loop(app, async {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(())
    })
    .await
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

/// Next notification (frame with `method`) within `timeout`, stashing responses.
async fn next_notification(ws: &mut Ws, inbox: &mut Inbox, timeout: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                match value.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        inbox.0.insert(id, value);
                    }
                    None if value.get("method").is_some() => return Some(value),
                    None => {}
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
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

async fn connect(session: &helix_claude_ide::Session, pid: u32) -> anyhow::Result<Ws> {
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
    send(&mut ws, 0, "initialize", json!({"protocolVersion": "2025-11-25"})).await?;
    wait_response(&mut ws, &mut inbox, 0).await?;
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"ide_connected","params":{"pid":pid}})
            .to_string()
            .into(),
    ))
    .await?;
    // A round trip orders us behind the notification.
    send(&mut ws, 1, "ping", json!({})).await?;
    wait_response(&mut ws, &mut inbox, 1).await?;
    Ok(ws)
}

fn open_diff(path: &std::path::Path, tab: &str, contents: &str) -> Value {
    json!({"name": "openDiff", "arguments": {
        "old_file_path": path.to_string_lossy(),
        "new_file_path": path.to_string_lossy(),
        "new_file_contents": contents,
        "tab_name": tab,
    }})
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_share_one_helix() -> anyhow::Result<()> {
    let config_dir = tempfile::tempdir()?;
    std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path());

    let dir = tempfile::tempdir()?;
    let file_a = std::fs::canonicalize(dir.path())?.join("alpha.rs");
    let file_b = std::fs::canonicalize(dir.path())?.join("beta.rs");
    std::fs::write(&file_a, "fn a() {}\n")?;
    std::fs::write(&file_b, "fn b() {}\n")?;

    let mut config = helix_term::config::Config::default();
    config.editor.claude_ide.diff_mode = helix_view::editor::ClaudeIdeDiffMode::Split;
    config.editor.claude_ide.max_clients = 2;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_claude_ide(Some("Helix-multi"))
        .with_file(file_a.clone(), None)
        .build()?;
    let session = app.editor.claude_ide.clone().expect("IDE server started");
    assert_eq!(session.handle.max_clients(), 2);
    assert!(!session.handler.exclusive_display(), "split mode: proposals coexist");

    let mut ws1 = connect(&session, 1111).await?;
    let mut ws2 = connect(&session, 2222).await?;
    let mut in1 = Inbox::default();
    let mut in2 = Inbox::default();
    assert_eq!(session.client_count(), 2);
    let clients = session.clients();
    assert_eq!(clients[0].pid, Some(1111));
    assert_eq!(clients[1].pid, Some(2222));
    let (c1, c2) = (clients[0].id, clients[1].id);
    assert_eq!((c1, c2), (ClientId(1), ClientId(2)));

    // A third connection is refused with 503 (max-clients = 2).
    {
        let lock: Value =
            serde_json::from_str(&std::fs::read_to_string(session.handle.lock_path())?)?;
        let uri: Uri = format!("ws://127.0.0.1:{}", session.port()).parse()?;
        let request = ClientRequestBuilder::new(uri)
            .with_sub_protocol("mcp")
            .with_header(
                "X-Claude-Code-Ide-Authorization",
                lock["authToken"].as_str().unwrap().to_string(),
            );
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status().as_u16(), 503)
            }
            other => panic!("expected 503, got {other:?}"),
        }
        assert_eq!(session.client_count(), 2);
    }

    // Drain the 500 ms selection replays both clients get after connecting.
    settle(&mut app, 700).await?;
    while next_notification(&mut ws1, &mut in1, Duration::from_millis(100))
        .await
        .is_some()
    {}
    while next_notification(&mut ws2, &mut in2, Duration::from_millis(100))
        .await
        .is_some()
    {}

    // ── :claude-mention with two clients and no focus opens a picker ──────────
    assert_eq!(session.handler.focus(), None);
    assert!(!app.has_layer("picker"));
    keys(&mut app, ":claude-mention<ret>").await?;
    settle(&mut app, 200).await?;
    assert!(app.has_layer("picker"), "two clients, no focus: ask, don't guess");
    keys(&mut app, "<esc>").await?;
    settle(&mut app, 100).await?;
    assert!(!app.has_layer("picker"));
    assert!(next_notification(&mut ws1, &mut in1, Duration::from_millis(200))
        .await
        .is_none());
    assert!(next_notification(&mut ws2, &mut in2, Duration::from_millis(200))
        .await
        .is_none());

    // ── :claude-ide-focus #2 → :claude-mention addresses only client 2 ────────
    keys(&mut app, ":claude-ide-focus #2<ret>").await?;
    assert_eq!(session.handler.focus(), Some(c2));
    assert_eq!(session.handler.active_client(), Some(c2));
    keys(&mut app, ":claude-mention<ret>").await?;
    let mention = with_loop(&mut app, async {
        next_notification(&mut ws2, &mut in2, Duration::from_secs(3))
            .await
            .ok_or_else(|| anyhow::anyhow!("client 2 expected at_mentioned"))
    })
    .await?;
    assert_eq!(mention["method"], "at_mentioned");
    assert_eq!(
        mention["params"]["filePath"].as_str().unwrap(),
        file_a.to_string_lossy()
    );
    assert!(
        next_notification(&mut ws1, &mut in1, Duration::from_millis(300))
            .await
            .is_none(),
        "client 1 must not be mentioned"
    );

    // Explicit pid argument overrides the focus.
    keys(&mut app, ":claude-mention 1111<ret>").await?;
    let mention = with_loop(&mut app, async {
        next_notification(&mut ws1, &mut in1, Duration::from_secs(3))
            .await
            .ok_or_else(|| anyhow::anyhow!("client 1 expected at_mentioned"))
    })
    .await?;
    assert_eq!(mention["method"], "at_mentioned");
    assert!(next_notification(&mut ws2, &mut in2, Duration::from_millis(300))
        .await
        .is_none());

    keys(&mut app, ":claude-ide-focus none<ret>").await?;
    assert_eq!(session.handler.focus(), None);
    // No focus, no diffs shown yet: the newest client is the default target.
    assert_eq!(session.handler.active_client(), Some(c2));

    // ── two proposals from two clients coexist as two splits ──────────────────
    send(&mut ws1, 10, "tools/call", open_diff(&file_a, "tab-a", "fn a() {}\nfn a2() {}\n")).await?;
    settle(&mut app, 300).await?;
    send(&mut ws2, 20, "tools/call", open_diff(&file_b, "tab-b", "fn b() {}\nfn b2() {}\n")).await?;
    settle(&mut app, 300).await?;

    assert_eq!(app.editor.claude_diff_views.len(), 2, "both splits are open");
    assert_eq!(session.handler.pending_count_for(c1), 1);
    assert_eq!(session.handler.pending_count_for(c2), 1);
    let view_a = app.editor.claude_diff_views[0].clone();
    let view_b = app.editor.claude_diff_views[1].clone();
    assert_eq!((view_a.client.id, view_a.client.pid), (1, Some(1111)));
    assert_eq!((view_b.client.id, view_b.client.pid), (2, Some(2222)));
    let name = |id: helix_view::DocumentId, app: &helix_term::application::Application| {
        app.editor
            .documents
            .get(&id)
            .unwrap()
            .path()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(name(view_a.right, &app), "\u{273B} alpha.rs [1111]");
    assert_eq!(name(view_b.right, &app), "\u{273B} beta.rs [2222]");
    assert_eq!(
        app.editor.documents.get(&view_a.right).unwrap().language_name(),
        Some("rust")
    );
    // The last shown proposal is client 2's: it becomes the default target.
    assert_eq!(session.handler.last_shown_client(), Some(c2));

    // Accepting A's proposal (focus on its right buffer) leaves B untouched.
    app.editor.focus(view_a.views[1]);
    assert_eq!(app.editor.tree.get(app.editor.tree.focus).doc, view_a.right);
    let reply = with_keys_loop(
        &mut app,
        ":claude-diff-accept<ret>",
        wait_response(&mut ws1, &mut in1, 10),
    )
    .await?;
    assert_eq!(reply["result"]["content"][0]["text"], "FILE_SAVED");
    assert_eq!(reply["result"]["content"][1]["text"], "fn a() {}\nfn a2() {}\n");
    assert_eq!(app.editor.claude_diff_views.len(), 1);
    assert_eq!(app.editor.claude_diff_views[0].tab_name, "tab-b");
    assert_eq!(session.handler.pending_count_for(c1), 0);
    assert_eq!(session.handler.pending_count_for(c2), 1);
    assert_eq!(std::fs::read_to_string(&file_a)?, "fn a() {}\n", "Helix never writes");

    // closeAllDiffTabs from client 1 (start of its next turn) does not touch B.
    send(&mut ws1, 11, "tools/call", json!({"name": "closeAllDiffTabs", "arguments": {}})).await?;
    let closed = with_loop(&mut app, wait_response(&mut ws1, &mut in1, 11)).await?;
    assert_eq!(closed["result"]["content"][0]["text"], "CLOSED_0_DIFF_TABS");
    settle(&mut app, 200).await?;
    assert_eq!(app.editor.claude_diff_views.len(), 1);
    assert_eq!(session.handler.pending_count_for(c2), 1);
    assert!(in2.0.get(&20).is_none(), "B's openDiff is still pending");

    // close_tab from client 1 with B's tab name: TAB_CLOSED, B still pending.
    send(&mut ws1, 12, "tools/call", json!({"name": "close_tab", "arguments": {"tab_name": "tab-b"}})).await?;
    let closed = with_loop(&mut app, wait_response(&mut ws1, &mut in1, 12)).await?;
    assert_eq!(closed["result"]["content"][0]["text"], "TAB_CLOSED");
    settle(&mut app, 200).await?;
    assert_eq!(app.editor.claude_diff_views.len(), 1);

    // ── :claude-ide-disconnect #2 closes client 2 and rejects its proposal ────
    keys(&mut app, ":claude-ide-disconnect #2<ret>").await?;
    let closed = with_loop(&mut app, async {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ws2.next()).await? {
                Some(Ok(Message::Close(frame))) => return Ok(frame),
                None | Some(Err(_)) => return Ok(None),
                Some(Ok(_)) => continue,
            }
        }
    })
    .await?;
    if let Some(frame) = closed {
        assert_eq!(u16::from(frame.code), 1000);
        assert_eq!(frame.reason.as_str(), "Closed by user");
    }
    settle(&mut app, 300).await?;
    assert_eq!(session.client_count(), 1);
    assert_eq!(session.clients()[0].id, c1);
    assert!(app.editor.claude_diff_views.is_empty(), "client 2's split is torn down");
    assert_eq!(session.handler.pending_diff_count(), 0);
    assert_eq!(session.handler.active_client(), Some(c1));
    assert_eq!(std::fs::read_to_string(&file_b)?, "fn b() {}\n");

    // Client 1 keeps working.
    send(&mut ws1, 13, "tools/call", json!({"name": "closeAllDiffTabs", "arguments": {}})).await?;
    let ok = with_loop(&mut app, wait_response(&mut ws1, &mut in1, 13)).await?;
    assert_eq!(ok["result"]["content"][0]["text"], "CLOSED_0_DIFF_TABS");

    app.close().await;
    Ok(())
}
