//! `selection_changed` / `at_mentioned` delivery through a real WebSocket
//! client, using the editor-backed handler (PROTO §3.5, §6).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;

use helix_claude_ide::notify::{Position, SelectionInfo};
use helix_claude_ide::{Config, EditorHandler, LockFile};

async fn next_notification(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    timeout: Duration,
) -> Option<Value> {
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
    assert!(!handler.mention(std::path::Path::new("/w/a.rs"), None));

    let lock: LockFile =
        serde_json::from_str(&std::fs::read_to_string(handle.lock_path()).unwrap()).unwrap();
    let uri: Uri = format!("ws://127.0.0.1:{}", handle.port()).parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_sub_protocol("mcp")
        .with_header("X-Claude-Code-Ide-Authorization", lock.auth_token);
    let started = tokio::time::Instant::now();
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();

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

    // at_mentioned is immediate.
    assert!(handler.mention(std::path::Path::new("/w/src/lib.rs"), Some((9, 14))));
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
