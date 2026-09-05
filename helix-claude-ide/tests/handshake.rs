//! End-to-end transport tests against a real `tokio-tungstenite` client,
//! covering PROTO §1.1, §2 and §3, plus the T8 multi-client rules.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use helix_claude_ide::{Config, IdeServerHandle, LockFile, NotImplementedHandler};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    _dir: tempfile::TempDir,
    handle: IdeServerHandle,
    token: String,
}

async fn start() -> Fixture {
    start_with(helix_claude_ide::DEFAULT_MAX_CLIENTS).await
}

async fn start_with(max_clients: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new("/tmp/workspace", "Helix-test");
    config.pid = 4242;
    config.lock_dir = Some(dir.path().join("ide"));
    config.max_clients = max_clients;
    let handle = helix_claude_ide::start(config, Arc::new(NotImplementedHandler))
        .await
        .unwrap();
    let lock: LockFile =
        serde_json::from_str(&std::fs::read_to_string(handle.lock_path()).unwrap()).unwrap();
    Fixture {
        _dir: dir,
        token: lock.auth_token,
        handle,
    }
}

fn request(port: u16, token: Option<&str>) -> ClientRequestBuilder {
    let uri: Uri = format!("ws://127.0.0.1:{port}").parse().unwrap();
    let mut builder = ClientRequestBuilder::new(uri).with_sub_protocol("mcp");
    if let Some(token) = token {
        builder = builder.with_header("X-Claude-Code-Ide-Authorization", token);
    }
    builder
}

async fn connect(fx: &Fixture) -> Ws {
    let (ws, _) = connect_async(request(fx.handle.port(), Some(&fx.token)))
        .await
        .unwrap();
    ws
}

async fn rpc(ws: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for response")
            .expect("stream ended")
            .unwrap();
        if let Message::Text(text) = msg {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_token_is_closed_with_1008() {
    let fx = start().await;
    let (mut ws, _) = connect_async(request(fx.handle.port(), Some("nope")))
        .await
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match msg {
        Message::Close(Some(frame)) => {
            assert_eq!(u16::from(frame.code), 1008);
            assert_eq!(frame.code, CloseCode::Policy);
            assert_eq!(frame.reason.as_str(), "Unauthorized");
        }
        other => panic!("expected close frame, got {other:?}"),
    }
    assert!(!fx.handle.is_connected());
    fx.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_token_is_closed_with_1008() {
    let fx = start().await;
    let (mut ws, _) = connect_async(request(fx.handle.port(), None))
        .await
        .unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    assert!(matches!(msg, Message::Close(Some(f)) if f.code == CloseCode::Policy));
    fx.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_echoes_mcp_subprotocol() {
    let fx = start().await;
    let (_ws, response) = connect_async(request(fx.handle.port(), Some(&fx.token)))
        .await
        .unwrap();
    assert_eq!(response.status(), 101);
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .map(|v| v.to_str().unwrap()),
        Some("mcp")
    );
    fx.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_and_tools_list() {
    let fx = start().await;
    let mut ws = connect(&fx).await;
    assert!(fx.handle.is_connected());

    let init = rpc(
        &mut ws,
        0,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"roots": {"listChanged": true}, "elicitation": {}},
            "clientInfo": {"name": "claude-code", "title": "Claude Code", "version": "2.1.261"}
        }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        init["result"]["capabilities"],
        json!({"tools": {"listChanged": true}})
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"],
        "Claude Code Helix MCP"
    );
    assert!(init["result"]["serverInfo"]["version"].is_string());

    // Notifications must not produce a response and must not break the session.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"ide_connected","params":{"pid":1}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    // Garbage frames are dropped, not fatal.
    ws.send(Message::Text("{not json".into())).await.unwrap();

    let fallback = rpc(
        &mut ws,
        1,
        "initialize",
        json!({"protocolVersion": "1999-01-01"}),
    )
    .await;
    assert_eq!(fallback["result"]["protocolVersion"], "2025-11-25");

    let list = rpc(&mut ws, 2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "closeAllDiffTabs",
            "close_tab",
            "getDiagnostics",
            "openDiff"
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
    }

    let ping = rpc(&mut ws, 3, "ping", json!({})).await;
    assert_eq!(ping["result"], json!({}));

    let unknown = rpc(
        &mut ws,
        4,
        "tools/call",
        json!({"name": "openFile", "arguments": {"filePath": "x"}}),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32602);
    assert_eq!(unknown["error"]["message"], "Tool openFile not found");

    let bad_args = rpc(
        &mut ws,
        5,
        "tools/call",
        json!({"name": "close_tab", "arguments": {}}),
    )
    .await;
    assert_eq!(bad_args["error"]["code"], -32602);

    let stub = rpc(
        &mut ws,
        6,
        "tools/call",
        json!({"name": "closeAllDiffTabs", "arguments": {}}),
    )
    .await;
    assert_eq!(stub["result"]["isError"], true);
    assert_eq!(stub["result"]["content"][0]["type"], "text");

    let no_method = rpc(&mut ws, 7, "prompts/list", json!({})).await;
    assert_eq!(no_method["error"]["code"], -32601);

    fx.handle.stop().await;
}

async fn next_text(ws: &mut Ws, timeout: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(timeout, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                return Some(serde_json::from_str(text.as_str()).unwrap())
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// T8: several CLIs share one server; nobody is evicted.
#[tokio::test(flavor = "multi_thread")]
async fn two_clients_coexist() {
    let fx = start().await;
    let mut first = connect(&fx).await;
    let init = rpc(&mut first, 0, "initialize", json!({"protocolVersion": "2025-11-25"})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    let mut second = connect(&fx).await;
    let init = rpc(&mut second, 0, "initialize", json!({"protocolVersion": "2025-11-25"})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");

    assert_eq!(fx.handle.client_count(), 2);
    let ids: Vec<u64> = fx.handle.clients().iter().map(|c| c.id.0).collect();
    assert_eq!(ids, [1, 2]);

    // Both are fully functional.
    for ws in [&mut first, &mut second] {
        let list = rpc(ws, 1, "tools/list", json!({})).await;
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 4);
    }

    // The first client was not closed in the meantime.
    assert!(
        next_text(&mut first, Duration::from_millis(200)).await.is_none(),
        "first client must not receive anything unsolicited"
    );

    // Broadcast reaches both; a targeted frame reaches one.
    assert_eq!(fx.handle.notify_all("selection_changed", json!({"text": "x"})), 2);
    for ws in [&mut first, &mut second] {
        let note = next_text(ws, Duration::from_secs(5)).await.expect("broadcast frame");
        assert_eq!(note["method"], "selection_changed");
        assert!(note.get("id").is_none());
    }
    let second_id = fx.handle.clients()[1].id;
    assert!(fx.handle.notify_one(second_id, "at_mentioned", json!({"filePath": "/w/a"})));
    let note = next_text(&mut second, Duration::from_secs(5)).await.expect("targeted frame");
    assert_eq!(note["method"], "at_mentioned");
    assert!(next_text(&mut first, Duration::from_millis(300)).await.is_none());

    // Closing one from the IDE side leaves the other untouched.
    assert!(fx.handle.close_client(fx.handle.clients()[0].id, "Closed by user"));
    let msg = tokio::time::timeout(Duration::from_secs(5), first.next())
        .await
        .expect("first client should be closed");
    match msg {
        Some(Ok(Message::Close(frame))) => {
            let frame = frame.unwrap();
            assert_eq!(u16::from(frame.code), 1000);
            assert_eq!(frame.reason.as_str(), "Closed by user");
        }
        None | Some(Err(_)) => {}
        other => panic!("expected close, got {other:?}"),
    }
    for _ in 0..50 {
        if fx.handle.client_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fx.handle.client_count(), 1);
    let pong = rpc(&mut second, 2, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    assert!(!fx.handle.close_client(helix_claude_ide::ClientId(99), "nope"));

    fx.handle.stop().await;
}

/// T8: over `max-clients` the upgrade is refused with HTTP 503 — no eviction,
/// so two auto-reconnecting CLIs never ping-pong.
#[tokio::test(flavor = "multi_thread")]
async fn max_clients_rejected_with_503() {
    let fx = start_with(1).await;
    assert_eq!(fx.handle.max_clients(), 1);
    let mut first = connect(&fx).await;
    let _ = rpc(&mut first, 0, "ping", json!({})).await;

    let refused = connect_async(request(fx.handle.port(), Some(&fx.token))).await;
    match refused {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 503);
            let body = response.body().as_deref().unwrap_or_default();
            assert_eq!(std::str::from_utf8(body).unwrap(), "too many clients");
        }
        other => panic!("expected HTTP 503, got {other:?}"),
    }
    assert_eq!(fx.handle.client_count(), 1);

    // The first client is untouched.
    let pong = rpc(&mut first, 1, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));

    // Once it leaves, the slot is free again.
    first.close(None).await.unwrap();
    for _ in 0..50 {
        if fx.handle.client_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut third = connect(&fx).await;
    let pong = rpc(&mut third, 0, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    assert_eq!(fx.handle.client_count(), 1);

    fx.handle.stop().await;
}

/// T8: `ide_connected {pid}` is remembered per connection.
#[tokio::test(flavor = "multi_thread")]
async fn ide_connected_sets_pid() {
    let fx = start().await;
    let mut ws = connect(&fx).await;
    let _ = rpc(&mut ws, 0, "initialize", json!({"protocolVersion": "2025-11-25"})).await;
    assert_eq!(fx.handle.clients()[0].pid, None);
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"ide_connected","params":{"pid":72464}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    // Notifications get no reply; a ping orders us behind it.
    let _ = rpc(&mut ws, 1, "ping", json!({})).await;
    let client = &fx.handle.clients()[0];
    assert_eq!(client.pid, Some(72464));
    assert_eq!(fx.handle.client(client.id).unwrap().pid, Some(72464));
    fx.handle.stop().await;
}

/// T9.1: `set_permission_mode` is accepted (`OK`), stored per client and
/// absent from `tools/list`.
#[tokio::test(flavor = "multi_thread")]
async fn set_permission_mode_ok_and_unlisted() {
    let fx = start().await;
    let mut ws = connect(&fx).await;
    let _ = rpc(&mut ws, 0, "initialize", json!({"protocolVersion": "2025-11-25"})).await;
    assert_eq!(fx.handle.clients()[0].permission_mode, None);

    let ok = rpc(
        &mut ws,
        1,
        "tools/call",
        json!({"name": "set_permission_mode", "arguments": {"mode": "acceptEdits"}}),
    )
    .await;
    assert_eq!(
        ok["result"],
        json!({"content": [{"type": "text", "text": "OK"}]})
    );
    assert_eq!(
        fx.handle.clients()[0].permission_mode.as_deref(),
        Some("acceptEdits")
    );

    let again = rpc(
        &mut ws,
        2,
        "tools/call",
        json!({"name": "set_permission_mode", "arguments": {"mode": "plan"}}),
    )
    .await;
    assert_eq!(again["result"]["content"][0]["text"], "OK");
    assert_eq!(fx.handle.clients()[0].permission_mode.as_deref(), Some("plan"));

    let bad = rpc(
        &mut ws,
        3,
        "tools/call",
        json!({"name": "set_permission_mode", "arguments": {}}),
    )
    .await;
    assert_eq!(bad["error"]["code"], -32602);

    let list = rpc(&mut ws, 4, "tools/list", json!({})).await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 4);
    assert!(!names.contains(&"set_permission_mode"));
    fx.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_clears_state() {
    let fx = start().await;
    let mut ws = connect(&fx).await;
    assert!(fx.handle.is_connected());
    ws.close(None).await.unwrap();
    for _ in 0..50 {
        if !fx.handle.is_connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!fx.handle.is_connected());
    assert!(!fx.handle.notify("selection_changed", json!({})));
    fx.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_file_lifecycle() {
    let fx = start().await;
    let path = fx.handle.lock_path().to_path_buf();
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        format!("{}.lock", fx.handle.port())
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.ends_with('\n'));
    let lock: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(lock["pid"], 4242);
    assert_eq!(lock["workspaceFolders"], json!(["/tmp/workspace"]));
    assert_eq!(lock["ideName"], "Helix-test");
    assert_eq!(lock["transport"], "ws");
    assert_eq!(lock["runningInWindows"], cfg!(windows));
    assert_eq!(lock["authToken"].as_str().unwrap().len(), 36);
    assert_eq!(lock.as_object().unwrap().len(), 6);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    let port = fx.handle.port();
    let mut ws = connect(&fx).await;
    fx.handle.stop().await;
    assert!(!path.exists(), "lock file must be removed by stop()");
    assert!(fx.handle.is_stopped());

    // Connected client is closed and the port is released.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "client was not closed by stop()");
    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        connect_async(request(port, Some(&fx.token))),
    )
    .await
    .unwrap();
    assert!(refused.is_err(), "server still accepting after stop()");

    // stop() is idempotent.
    fx.handle.stop().await;
}
