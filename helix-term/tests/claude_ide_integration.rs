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
use test::helpers::AppBuilder;

#[tokio::test(flavor = "multi_thread")]
async fn claude_ide_server_lifecycle() -> anyhow::Result<()> {
    // Keep lock files out of the real `~/.claude/ide`.
    let config_dir = tempfile::tempdir()?;
    std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path());

    let mut app = AppBuilder::new()
        .with_claude_ide(Some("Helix-integration"))
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

    // Closing the editor stops the server and removes the lock file.
    let errs = app.close().await;
    assert!(errs.is_empty(), "close() reported errors: {errs:?}");
    assert!(app.editor.claude_ide.is_none());
    assert!(!lock_path.exists(), "lock file must be removed on close");
    assert!(session.handle.is_stopped());
    Ok(())
}
