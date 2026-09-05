//! `selection_changed` / `at_mentioned` delivery through real WebSocket
//! clients, using the editor-backed handler (PROTO §3.5, §6) — broadcast to
//! every client, replay per connection, mention to one client (T8).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;

use helix_claude_ide::notify::{Position, SelectionInfo};
use helix_claude_ide::{ClientId, Config, EditorHandler, IdeServerHandle, LockFile};

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn next_notification(ws: &mut Ws, timeout: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                if value.get("method").is_some() {
                    return Some(value);
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

async fn connect(handle: &IdeServerHandle) -> Ws {
    let lock: LockFile =
        serde_json::from_str(&std::fs::read_to_string(handle.lock_path()).unwrap()).unwrap();
    let uri: Uri = format!("ws://127.0.0.1:{}", handle.port()).parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_sub_protocol("mcp")
        .with_header("X-Claude-Code-Ide-Authorization", lock.auth_token);
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

fn selection(line: usize, text: &str) -> SelectionInfo {
    SelectionInfo {
        file_path: PathBuf::from("/w/src/lib.rs"),
        text: text.to_string(),
        start: Position { line, character: 0 },
        end: Position {
            line,
            character: text.chars().count(),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn selection_and_mention_reach_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let (mcp_tx, _mcp_rx) = tokio::sync::mpsc::channel(4);
    let handler = Arc::new(EditorHandler::new(mcp_tx));
    let mut config = Config::new("/w", "Helix-notify");
    config.lock_dir = Some(dir.path().join("ide"));
    let handle = helix_claude_ide::start(config, handler.clone())
        .await
        .unwrap();

    // Selection recorded before any client exists is replayed after connect.
    handler.selection_changed(selection(3, "let x = 1;"));
    assert!(handler.clients().is_empty());
    assert_eq!(handler.active_client(), None);
    assert!(!handler.mention(ClientId(1), std::path::Path::new("/w/a.rs"), None));
    // Let the 300 ms debounce fire into the void first (nobody connected).
    tokio::time::sleep(Duration::from_millis(400)).await;

    let started = tokio::time::Instant::now();
    let mut ws = connect(&handle).await;

    let replay = next_notification(&mut ws, Duration::from_secs(3))
        .await
        .expect("cached selection replayed");
    let elapsed = started.elapsed();
    assert_eq!(replay["method"], "selection_changed");
    assert_eq!(replay["params"]["text"], "let x = 1;");
    assert_eq!(replay["params"]["selection"]["start"]["line"], 3);
    assert!(
        elapsed >= Duration::from_millis(450),
        "replay must wait ~500 ms, took {elapsed:?}"
    );

    // Two quick changes collapse into one frame with the latest value.
    handler.selection_changed(selection(7, "a"));
    handler.selection_changed(selection(8, "bb"));
    let frame = next_notification(&mut ws, Duration::from_secs(3))
        .await
        .expect("debounced selection");
    assert_eq!(frame["params"]["selection"]["start"]["line"], 8);
    assert_eq!(frame["params"]["fileUrl"], "file:///w/src/lib.rs");
    assert!(
        next_notification(&mut ws, Duration::from_millis(500))
            .await
            .is_none(),
        "only one frame for two rapid changes"
    );

    // Same selection again: nothing.
    handler.selection_changed(selection(8, "bb"));
    assert!(next_notification(&mut ws, Duration::from_millis(500))
        .await
        .is_none());

    // at_mentioned is immediate; with one client it is the active one.
    let target = handler.active_client().expect("one client is active");
    assert_eq!(target, ClientId(1));
    assert!(handler.mention(target, std::path::Path::new("/w/src/lib.rs"), Some((9, 14))));
    let mention = next_notification(&mut ws, Duration::from_secs(3))
        .await
        .expect("at_mentioned frame");
    assert_eq!(mention["method"], "at_mentioned");
    assert_eq!(
        mention["params"],
        json!({"filePath": "/w/src/lib.rs", "lineStart": 9, "lineEnd": 14})
    );

    handle.stop().await;
}

/// T8: the replay goes to the new connection only; changes are broadcast;
/// mentions and focus address one client.
#[tokio::test(flavor = "multi_thread")]
async fn replay_per_connection_and_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    let (mcp_tx, _mcp_rx) = tokio::sync::mpsc::channel(4);
    let handler = Arc::new(EditorHandler::new(mcp_tx));
    let mut config = Config::new("/w", "Helix-notify-2");
    config.lock_dir = Some(dir.path().join("ide"));
    let handle = helix_claude_ide::start(config, handler.clone())
        .await
        .unwrap();

    let mut first = connect(&handle).await;
    // Nothing cached yet: no replay for the first client.
    assert!(next_notification(&mut first, Duration::from_millis(700))
        .await
        .is_none());

    handler.selection_changed(selection(1, "one"));
    let frame = next_notification(&mut first, Duration::from_secs(3))
        .await
        .expect("first client gets the change");
    assert_eq!(frame["params"]["text"], "one");

    // Second client: replay after ~500 ms, first client hears nothing.
    let started = tokio::time::Instant::now();
    let mut second = connect(&handle).await;
    let replay = next_notification(&mut second, Duration::from_secs(3))
        .await
        .expect("second client gets the cached selection");
    assert!(started.elapsed() >= Duration::from_millis(450));
    assert_eq!(replay["params"]["text"], "one");
    assert!(
        next_notification(&mut first, Duration::from_millis(300))
            .await
            .is_none(),
        "replay is per connection"
    );
    assert_eq!(handler.client_count(), 2);
    let ids: Vec<ClientId> = handler.clients().iter().map(|c| c.id).collect();
    assert_eq!(ids, [ClientId(1), ClientId(2)]);

    // A new selection is broadcast to both.
    handler.selection_changed(selection(2, "two"));
    for ws in [&mut first, &mut second] {
        let frame = next_notification(ws, Duration::from_secs(3))
            .await
            .expect("broadcast");
        assert_eq!(frame["params"]["text"], "two");
    }

    // Default target without focus or shown diffs: the newest client.
    assert_eq!(handler.focus(), None);
    assert_eq!(handler.active_client(), Some(ClientId(2)));
    // Explicit focus wins; #N and pid forms resolve.
    handler.set_focus(Some(ClientId(1))).unwrap();
    assert_eq!(handler.focus(), Some(ClientId(1)));
    assert_eq!(handler.active_client(), Some(ClientId(1)));
    assert!(handler.set_focus(Some(ClientId(9))).is_err());
    assert_eq!(handler.resolve_client_arg("#2").unwrap(), ClientId(2));
    assert!(handler.resolve_client_arg("#7").is_err());
    assert!(handler.resolve_client_arg("4242").is_err());
    assert!(handler.resolve_client_arg("zzz").is_err());

    // Mention goes to one client only.
    assert!(handler.mention(ClientId(2), std::path::Path::new("/w/b.rs"), None));
    let mention = next_notification(&mut second, Duration::from_secs(3))
        .await
        .expect("mention for the second client");
    assert_eq!(mention["method"], "at_mentioned");
    assert!(next_notification(&mut first, Duration::from_millis(300))
        .await
        .is_none());

    // Focus is dropped when its client leaves.
    first.close(None).await.unwrap();
    for _ in 0..50 {
        if handler.client_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(handler.focus(), None);
    assert_eq!(handler.active_client(), Some(ClientId(2)));
    assert!(!handler.mention(ClientId(1), std::path::Path::new("/w/b.rs"), None));

    handle.stop().await;
}
