//! Minimal CLI-side client for manual checks against a running Helix:
//!
//! ```text
//! cargo run -p helix-claude-ide --example client -- <port.lock> tools/list
//! cargo run -p helix-claude-ide --example client -- <port.lock> getDiagnostics '{"uri":"file:///abs/path.rs"}'
//! cargo run -p helix-claude-ide --example client -- <port.lock> closeAllDiffTabs
//! ```
//!
//! Reads the auth token from the lock file, performs `initialize`, then
//! either lists tools or calls one and prints the raw JSON-RPC response.

use std::path::PathBuf;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let lock_path = PathBuf::from(
        args.next()
            .expect("usage: client <port.lock> <method|tool> [json-args]"),
    );
    let what = args.next().unwrap_or_else(|| "tools/list".to_string());
    let tool_args: Value = match args.next() {
        Some(text) => serde_json::from_str(&text)?,
        None => json!({}),
    };

    let port: u16 = lock_path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse().ok())
        .expect("lock file name must be <port>.lock");
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(&lock_path)?)?;
    let token = lock["authToken"].as_str().unwrap_or_default().to_string();

    let uri: Uri = format!("ws://127.0.0.1:{port}").parse()?;
    let request = ClientRequestBuilder::new(uri)
        .with_sub_protocol("mcp")
        .with_header("X-Claude-Code-Ide-Authorization", token);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;

    let mut next_id = 0u64;
    let mut call = |method: &str, params: Value| {
        let id = next_id;
        next_id += 1;
        (
            id,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
        )
    };

    let (_, init) = call("initialize", json!({"protocolVersion":"2025-11-25"}));
    ws.send(Message::Text(init.into())).await?;
    let _ = ws.next().await;
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            .to_string()
            .into(),
    ))
    .await?;

    let (id, frame) = if what.contains('/') {
        call(&what, tool_args)
    } else {
        call("tools/call", json!({"name": what, "arguments": tool_args}))
    };
    let started = std::time::Instant::now();
    ws.send(Message::Text(frame.into())).await?;
    while let Some(msg) = ws.next().await {
        if let Message::Text(text) = msg? {
            let value: Value = serde_json::from_str(text.as_str())?;
            if value.get("id") == Some(&json!(id)) {
                eprintln!("({} ms)", started.elapsed().as_millis());
                println!("{}", serde_json::to_string_pretty(&value)?);
                break;
            }
            eprintln!("notification: {text}");
        }
    }
    let _ = ws.close(None).await;
    Ok(())
}
