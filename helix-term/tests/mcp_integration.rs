//! Integration tests for the helix-mcp toolset.
//!
//! Two groups:
//!
//! **Group 1 – Filesystem tools** (ping, list_dir, find_files, search, read_file-disk,
//! read_range, error cases): each test starts its own MCP server via
//! `helix_mcp::run_mcp_server` without creating a full `Application`.  This avoids the
//! global `MCP_EDITOR_TX` OnceLock and lets these tests run in parallel.
//!
//! **Group 2 – Editor-integrated tools** (read_file-buffer, get_open_buffers,
//! write_file, edit_file, insert_text, get_cursor, get_viewport): all assertions
//! live inside *one* `#[tokio::test]` function that owns the single `Application`
//! instance.  The McpCommand channel sender is stored in a global OnceLock, so only
//! the first Application ever created receives commands; serialising these tests into
//! one function keeps things correct.
//!
//! Run:
//!   cargo test --package helix-term --features integration --test mcp_integration -- --nocapture
//!
//! Single test:
//!   cargo test --package helix-term --features integration --test mcp_integration ping -- --nocapture

#![cfg(feature = "integration")]

use std::{
    fs,
    future::Future,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context};
use serde_json::{json, Value};

use helix_term::application::Application;

// Bring the integration-test helpers into scope.
// Bring the integration-test helpers into scope the same way integration.rs does.
mod test {
    pub mod helpers;
}
use test::helpers::{run_event_loop_until_idle, AppBuilder};
// ─── MCP HTTP client ──────────────────────────────────────────────────────────

struct McpClient {
    client: reqwest::Client,
    url: String,
    session_id: std::sync::Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl McpClient {
    /// Connect to an already-running MCP server and complete the MCP handshake.
    async fn connect(port: u16) -> anyhow::Result<Self> {
        let mc = Self {
            client: reqwest::Client::new(),
            url: format!("http://127.0.0.1:{port}/mcp"),
            session_id: std::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        };
        mc.initialize().await?;
        Ok(mc)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// POST a JSON-RPC message and return the parsed JSON body.
    async fn post_rpc(&self, body: Value) -> anyhow::Result<Value> {
        let session = self.session_id.lock().unwrap().clone();
        let mut builder = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(id) = session {
            builder = builder.header("Mcp-Session-Id", id);
        }

        let resp = builder.send().await.context("HTTP send")?;

        // Persist the session ID returned by the server.
        if let Some(v) = resp.headers().get("Mcp-Session-Id") {
            if let Ok(s) = v.to_str() {
                *self.session_id.lock().unwrap() = Some(s.to_string());
            }
        }

        let ct = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp.text().await.context("read body")?;

        if text.is_empty() {
            return Ok(json!(null));
        }
        if ct.contains("text/event-stream") {
            parse_sse(&text)
        } else {
            serde_json::from_str(&text).context("parse JSON response")
        }
    }

    /// Perform the MCP `initialize` + `notifications/initialized` handshake.
    async fn initialize(&self) -> anyhow::Result<()> {
        let id = self.next_id();
        let resp = self
            .post_rpc(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "helix-mcp-test", "version": "0.1.0" }
                }
            }))
            .await?;
        if resp.get("error").is_some() {
            bail!("initialize failed: {}", resp["error"]);
        }

        // The `initialized` notification must be sent; a 202/empty reply is normal.
        let _ = self
            .post_rpc(json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        Ok(())
    }

    /// Call a tool and return the *content* JSON
    /// (`result.content[0].text` parsed as JSON).
    async fn call(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let id = self.next_id();
        let resp = self
            .post_rpc(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": args }
            }))
            .await?;
        extract_content(&resp)
    }

    /// Like `call`, but returns the raw JSON-RPC envelope (preserves error info).
    async fn call_raw(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let id = self.next_id();
        self.post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }))
        .await
    }
}

// ─── Protocol helpers ─────────────────────────────────────────────────────────

/// Return the first non-empty `data:` line from an SSE response body.
fn parse_sse(text: &str) -> anyhow::Result<Value> {
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if !data.is_empty() {
                return serde_json::from_str(data).context("SSE data not valid JSON");
            }
        }
    }
    bail!(
        "no data line in SSE response (first 300 chars): {:?}",
        &text[..text.len().min(300)]
    )
}

/// Extract the tool result from a JSON-RPC response.
/// `result.content[0].text` is a JSON string; parse and return it.
fn extract_content(resp: &Value) -> anyhow::Result<Value> {
    if let Some(err) = resp.get("error") {
        bail!("JSON-RPC error: {err}");
    }
    let result = &resp["result"];
    if let Some(text) = result["content"][0]["text"].as_str() {
        // Try to parse as JSON first (most tools return JSON-encoded data).
        // Fall back to a plain string for tools like `ping` that return bare text.
        return Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())));
    }
    Ok(result.clone())
}

// ─── Fixtures ─────────────────────────────────────────────────────────────────

fn create_fixtures(dir: &Path) -> anyhow::Result<()> {
    fs::write(dir.join("hello.txt"), "Hello, world!\nLine two\nLine three\n")?;
    fs::create_dir_all(dir.join("sub"))?;
    fs::write(dir.join("sub").join("nested.txt"), "nested content\n")?;
    fs::write(dir.join("numbers.txt"), "alpha\nbeta\ngamma\n")?;
    Ok(())
}

// ─── Event-loop helper ────────────────────────────────────────────────────────

/// Run a future `f` while cycling the editor event loop concurrently.
///
/// Tools that dispatch through the `McpCommand` channel require the editor's
/// event loop to be active so it can receive and answer the command.  This
/// function runs the loop in a `select!` branch that is cancelled as soon as
/// `f` resolves.
async fn with_loop<F: Future<Output = anyhow::Result<Value>>>(
    app: &mut Application,
    f: F,
) -> anyhow::Result<Value> {
    tokio::select! {
        result = f => result,
        _ = async {
            loop {
                run_event_loop_until_idle(app).await;
                tokio::task::yield_now().await;
            }
        } => unreachable!(),
    }
}

/// Start a standalone MCP server (no Application, no editor channel).
/// Filesystem tools work correctly; editor-channel tools fall back gracefully.
async fn start_mcp_server() -> anyhow::Result<u16> {
    let addr = helix_mcp::run_mcp_server(None).await?;
    Ok(addr.port())
}

// ══════════════════════════════════════════════════════════════════════════════
// GROUP 1 — Filesystem tools (no Application, no editor channel)
// ══════════════════════════════════════════════════════════════════════════════

// ── 01: ping ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_ping() -> anyhow::Result<()> {
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client.call("ping", json!({})).await?;

    assert_eq!(
        result.as_str().unwrap_or(""),
        "pong",
        "ping should return \"pong\""
    );
    Ok(())
}

// ── 02: list_dir flat ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_list_dir_flat() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call("list_dir", json!({ "path": dir.path(), "recursive": false }))
        .await?;

    let entries = result["entries"].as_array().context("entries array")?;
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["path"].as_str())
        .map(|p| Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(""))
        .collect();

    assert!(names.contains(&"hello.txt"), "should contain hello.txt; got {names:?}");
    assert!(names.contains(&"numbers.txt"), "should contain numbers.txt");
    assert!(names.contains(&"sub"), "should contain sub/");
    assert!(
        !names.contains(&"nested.txt"),
        "flat listing should not recurse into sub/"
    );
    Ok(())
}

// ── 03: list_dir recursive ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_list_dir_recursive() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call("list_dir", json!({ "path": dir.path(), "recursive": true }))
        .await?;

    let entries = result["entries"].as_array().context("entries array")?;
    let paths: Vec<String> = entries
        .iter()
        .filter_map(|e| e["path"].as_str().map(String::from))
        .collect();

    assert!(
        paths.iter().any(|p| p.ends_with("nested.txt")),
        "recursive listing should include sub/nested.txt; paths: {paths:?}"
    );
    Ok(())
}

// ── 04: find_files – match ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_find_files_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call("find_files", json!({ "glob": "**/*.txt", "path": dir.path() }))
        .await?;

    let files = result["files"].as_array().context("files array")?;
    assert_eq!(files.len(), 3, "should find 3 .txt files; got {files:?}");

    let paths: Vec<&str> = files.iter().filter_map(|f| f.as_str()).collect();
    assert!(paths.iter().any(|p| p.ends_with("hello.txt")));
    assert!(paths.iter().any(|p| p.ends_with("numbers.txt")));
    assert!(paths.iter().any(|p| p.ends_with("nested.txt")));
    Ok(())
}

// ── 05: find_files – no match ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_find_files_no_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call("find_files", json!({ "glob": "**/*.rs", "path": dir.path() }))
        .await?;

    let files = result["files"].as_array().context("files array")?;
    assert!(files.is_empty(), "no .rs files should be found");
    Ok(())
}

// ── 06: search – pattern found ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_search_found() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call("search", json!({ "pattern": "Line", "path": dir.path() }))
        .await?;

    let matches = result["matches"].as_array().context("matches array")?;
    assert!(!matches.is_empty(), "should find matches for 'Line'");

    let texts: Vec<&str> = matches.iter().filter_map(|m| m["text"].as_str()).collect();
    assert!(
        texts.iter().any(|t| t.contains("Line two")),
        "should match 'Line two'; texts: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("Line three")),
        "should match 'Line three'"
    );
    Ok(())
}

// ── 07: search – case-sensitive no match ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_search_case_sensitive_no_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call(
            "search",
            json!({ "pattern": "line", "path": dir.path(), "case_sensitive": true }),
        )
        .await?;

    let matches = result["matches"].as_array().context("matches array")?;
    assert!(
        matches.is_empty(),
        "case-sensitive 'line' should not match 'Line'; got {matches:?}"
    );
    Ok(())
}

// ── 08: search – case-insensitive match ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_search_case_insensitive() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call(
            "search",
            json!({ "pattern": "line", "path": dir.path(), "case_sensitive": false }),
        )
        .await?;

    let matches = result["matches"].as_array().context("matches array")?;
    assert!(
        !matches.is_empty(),
        "case-insensitive 'line' should find matches"
    );
    Ok(())
}

// ── 09: search – with context_lines ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_search_context_lines() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let result = client
        .call(
            "search",
            json!({ "pattern": "two", "path": dir.path(), "context_lines": 1 }),
        )
        .await?;

    let matches = result["matches"].as_array().context("matches array")?;
    let m = matches
        .iter()
        .find(|m| m["text"].as_str().map_or(false, |t| t.contains("two")))
        .context("should find a match for 'two'")?;

    assert!(
        m["context_before"]
            .as_array()
            .map_or(false, |a| !a.is_empty()),
        "context_before should be non-empty"
    );
    assert!(
        m["context_after"]
            .as_array()
            .map_or(false, |a| !a.is_empty()),
        "context_after should be non-empty"
    );
    Ok(())
}

// ── 10: read_file – disk fallback ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_read_file_disk() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let path = dir.path().join("hello.txt");
    let result = client.call("read_file", json!({ "path": path })).await?;

    let text = result["content"][0]["text"]
        .as_str()
        .context("content[0].text")?;
    assert_eq!(text, "Hello, world!\nLine two\nLine three\n");
    assert_eq!(
        result["metadata"]["from_buffer"],
        json!(false),
        "disk read → from_buffer should be false"
    );
    Ok(())
}

// ── 12: read_range – two lines ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_read_range_two_lines() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let path = dir.path().join("hello.txt");
    let result = client
        .call(
            "read_range",
            json!({ "path": path, "start_line": 0, "end_line": 1 }),
        )
        .await?;

    let text = result["content"][0]["text"]
        .as_str()
        .context("content text")?;
    assert!(text.contains("Hello, world!"), "should contain line 1");
    assert!(text.contains("Line two"), "should contain line 2");
    assert!(!text.contains("Line three"), "should NOT contain line 3");
    Ok(())
}

// ── 13: read_range – single line ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_read_range_single_line() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let path = dir.path().join("hello.txt");
    let result = client
        .call(
            "read_range",
            json!({ "path": path, "start_line": 2, "end_line": 2 }),
        )
        .await?;

    let text = result["content"][0]["text"]
        .as_str()
        .context("content text")?;
    assert!(text.contains("Line three"), "should contain 'Line three' (line index 2)");
    assert!(!text.contains("Hello"), "should NOT contain line 1");
    assert!(!text.contains("Line two"), "should NOT contain line 2");
    Ok(())
}

// ── 22: read_file – nonexistent path ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_read_file_nonexistent() -> anyhow::Result<()> {
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    let resp = client
        .call_raw(
            "read_file",
            json!({ "path": "/nonexistent/path/__helix_mcp_test__.txt" }),
        )
        .await?;

    let is_error = resp.get("error").is_some()
        || resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        is_error,
        "reading a nonexistent file should produce an error; got: {resp}"
    );
    Ok(())
}

// ── 23: search – invalid regex ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_search_invalid_regex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let port = start_mcp_server().await?;
    let client = McpClient::connect(port).await?;

    // "[" is an invalid regular expression.
    let resp = client
        .call_raw("search", json!({ "pattern": "[", "path": dir.path() }))
        .await?;

    // Must not panic; either an error envelope or an error-flagged result is fine.
    let graceful = resp.get("error").is_some()
        || resp["result"]["isError"].as_bool().unwrap_or(false)
        || resp["result"]["content"].is_array();
    assert!(
        graceful,
        "invalid regex should be handled gracefully; got: {resp}"
    );
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// GROUP 2 — Editor-integrated tools
//
// All tests share ONE Application to avoid the MCP_EDITOR_TX OnceLock being
// set to a dead sender.  Assertions are sequential within the test function.
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread")]
async fn test_editor_tools() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    create_fixtures(dir.path())?;
    let hello = dir.path().join("hello.txt");

    // Build Application with MCP enabled (auto_approve=true for write tests).
    let mut app = AppBuilder::new()
        .with_mcp(true)
        .with_file(hello.clone(), None)
        .build()?;

    let port = app
        .editor
        .mcp_addr
        .context("MCP server should have started")?
        .port();

    // Initialize client (rmcp handles this in the background axum task).
    let client = McpClient::connect(port).await?;

    // ── 11: read_file – buffer read ────────────────────────────────────────
    {
        let result =
            with_loop(&mut app, client.call("read_file", json!({ "path": hello }))).await?;

        let text = result["content"][0]["text"]
            .as_str()
            .context("content text")?;
        assert_eq!(
            text,
            "Hello, world!\nLine two\nLine three\n",
            "read_file: wrong content"
        );
        assert_eq!(
            result["metadata"]["from_buffer"],
            json!(true),
            "file opened in editor → from_buffer must be true"
        );
        assert_eq!(
            result["metadata"]["line_count"],
            json!(3),
            "line_count should be 3"
        );
    }

    // ── 15: get_open_buffers – file open ──────────────────────────────────
    {
        let result =
            with_loop(&mut app, client.call("get_open_buffers", json!({}))).await?;

        let buffers = result["buffers"].as_array().context("buffers array")?;
        assert!(!buffers.is_empty(), "at least one buffer should be open");

        let buf = buffers
            .iter()
            .find(|b| {
                b["path"]
                    .as_str()
                    .map_or(false, |p| p.ends_with("hello.txt"))
            })
            .context("hello.txt should appear in open buffers")?;

        assert_eq!(
            buf["is_modified"],
            json!(false),
            "file should not be modified"
        );
    }

    // ── 20: get_cursor ────────────────────────────────────────────────────
    {
        let result = with_loop(&mut app, client.call("get_cursor", json!({}))).await?;

        assert!(result.get("line").is_some(), "get_cursor: missing 'line'");
        assert!(result.get("col").is_some(), "get_cursor: missing 'col'");
        assert_eq!(
            result["mode"].as_str().unwrap_or(""),
            "normal",
            "editor should be in normal mode"
        );
    }

    // ── 21: get_viewport ──────────────────────────────────────────────────
    {
        let result = with_loop(
            &mut app,
            client.call("get_viewport", json!({ "path": hello })),
        )
        .await?;

        let first = result["first_visible_line"]
            .as_u64()
            .context("first_visible_line")?;
        let last = result["last_visible_line"]
            .as_u64()
            .context("last_visible_line")?;

        assert!(first >= 1, "first_visible_line should be ≥ 1 (1-indexed)");
        assert!(last >= first, "last_visible_line should be ≥ first");
    }

    // ── 16: write_file – create new file ──────────────────────────────────
    {
        let new_path = dir.path().join("new.txt");
        let result = with_loop(
            &mut app,
            client.call(
                "write_file",
                json!({ "path": new_path, "content": "created\n" }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true), "write_file should save");
        assert_eq!(fs::read_to_string(&new_path)?, "created\n");
    }

    // ── 17: write_file – overwrite existing file ───────────────────────────
    {
        let result = with_loop(
            &mut app,
            client.call(
                "write_file",
                json!({ "path": hello, "content": "replaced\n" }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        assert_eq!(fs::read_to_string(&hello)?, "replaced\n");

        // Restore original content for subsequent subtests.
        with_loop(
            &mut app,
            client.call(
                "write_file",
                json!({ "path": hello, "content": "Hello, world!\nLine two\nLine three\n" }),
            ),
        )
        .await?;
    }

    // ── 18: edit_file – replace a single line ─────────────────────────────
    {
        // edit_file uses 1-indexed lines.
        let result = with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [{ "start_line": 2, "end_line": 2, "new_text": "REPLACED\n" }]
                }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let on_disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = on_disk.lines().collect();
        assert_eq!(
            lines.get(1).copied(),
            Some("REPLACED"),
            "line 2 (1-indexed) should be REPLACED"
        );

        // Restore.
        with_loop(
            &mut app,
            client.call(
                "write_file",
                json!({ "path": hello, "content": "Hello, world!\nLine two\nLine three\n" }),
            ),
        )
        .await?;
    }

    // ── 19: insert_text – prepend a line ──────────────────────────────────
    {
        // insert_text line param is 1-indexed; inserting before line 1 prepends.
        let result = with_loop(
            &mut app,
            client.call(
                "insert_text",
                json!({ "path": hello, "line": 1, "text": "HEADER\n" }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let on_disk = fs::read_to_string(&hello)?;
        let first = on_disk.lines().next().context("first line")?;
        assert_eq!(first, "HEADER", "first line should be HEADER after insert");
    }

    // ── Restore macro (used by W-A through W-J subtests) ──────────────────
    macro_rules! restore {
        () => {
            with_loop(
                &mut app,
                client.call(
                    "write_file",
                    json!({ "path": hello, "content": "Hello, world!\nLine two\nLine three\n" }),
                ),
            )
            .await?;
        };
    }

    // ── W-A: write_file full content + lines_changed + buffer round-trip ──
    {
        restore!();
        let five_lines = "one\ntwo\nthree\nfour\nfive\n";
        let result = with_loop(
            &mut app,
            client.call("write_file", json!({ "path": hello, "content": five_lines })),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        assert!(
            result["lines_changed"].as_u64().unwrap_or(0) > 0,
            "lines_changed should be >0"
        );
        assert_eq!(fs::read_to_string(&hello)?, five_lines);

        // Buffer round-trip: buffer must be reloaded after write.
        let rf =
            with_loop(&mut app, client.call("read_file", json!({ "path": hello }))).await?;
        assert_eq!(
            rf["content"][0]["text"].as_str().unwrap(),
            five_lines,
            "read_file buffer should reflect write_file result"
        );
        assert_eq!(rf["metadata"]["from_buffer"], json!(true));
    }

    // ── W-B: edit_file — unchanged lines preserved ────────────────────────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [{ "start_line": 2, "end_line": 2, "new_text": "REPLACED\n" }]
                }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 3, "line count must stay at 3");
        assert_eq!(lines[0], "Hello, world!", "line 1 must be unchanged");
        assert_eq!(lines[1], "REPLACED", "line 2 must be REPLACED");
        assert_eq!(lines[2], "Line three", "line 3 must be unchanged");
    }

    // ── W-C: edit_file — 1 line → 3 lines (file grows) ───────────────────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [{ "start_line": 2, "end_line": 2, "new_text": "A\nB\nC\n" }]
                }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 5, "file should grow from 3 to 5 lines");
        assert_eq!(lines[0], "Hello, world!");
        assert_eq!(lines[1], "A");
        assert_eq!(lines[2], "B");
        assert_eq!(lines[3], "C");
        assert_eq!(lines[4], "Line three");
    }

    // ── W-D: edit_file — delete a line (new_text = "") ───────────────────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [{ "start_line": 2, "end_line": 2, "new_text": "" }]
                }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 2, "file should shrink to 2 lines after deletion");
        assert_eq!(lines[0], "Hello, world!");
        assert_eq!(lines[1], "Line three");
    }

    // ── W-E: edit_file — two non-overlapping edits in one call ───────────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [
                        { "start_line": 1, "end_line": 1, "new_text": "First\n" },
                        { "start_line": 3, "end_line": 3, "new_text": "Third\n" }
                    ]
                }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "First", "line 1 must be First");
        assert_eq!(lines[1], "Line two", "line 2 must be unchanged");
        assert_eq!(lines[2], "Third", "line 3 must be Third");
    }

    // ── W-F: edit_file — pure insertion (end_line < start_line) ──────────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [{ "start_line": 2, "end_line": 1, "new_text": "INSERTED\n" }]
                }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 4, "file should grow to 4 lines");
        assert_eq!(lines[0], "Hello, world!");
        assert_eq!(lines[1], "INSERTED");
        assert_eq!(lines[2], "Line two");
        assert_eq!(lines[3], "Line three");
    }

    // ── W-G: insert_text — full file verification after middle insert ─────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "insert_text",
                json!({ "path": hello, "line": 2, "text": "MIDDLE\n" }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "Hello, world!");
        assert_eq!(lines[1], "MIDDLE");
        assert_eq!(lines[2], "Line two");
        assert_eq!(lines[3], "Line three");
    }

    // ── W-H: insert_text — append to end of file ─────────────────────────
    {
        restore!();
        let result = with_loop(
            &mut app,
            client.call(
                "insert_text",
                json!({ "path": hello, "line": 4, "text": "APPENDED\n" }),
            ),
        )
        .await?;

        assert_eq!(result["saved"], json!(true));
        let disk = fs::read_to_string(&hello)?;
        let lines: Vec<&str> = disk.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "Hello, world!", "first line unchanged");
        assert_eq!(lines[3], "APPENDED", "last line should be APPENDED");
    }

    // ── W-I: edit_file → read_file buffer reload (cross-tool) ────────────
    {
        restore!();
        with_loop(
            &mut app,
            client.call(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [{ "start_line": 1, "end_line": 1, "new_text": "BUFFERED\n" }]
                }),
            ),
        )
        .await?;

        let rf =
            with_loop(&mut app, client.call("read_file", json!({ "path": hello }))).await?;
        let text = rf["content"][0]["text"].as_str().context("content text")?;
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0], "BUFFERED",
            "editor buffer should reflect edit_file result immediately"
        );
        assert_eq!(
            rf["metadata"]["from_buffer"],
            json!(true),
            "file is open in editor, must read from buffer"
        );
    }

    // ── W-J: edit_file — overlapping edits rejected, file unchanged ───────
    {
        restore!();
        let resp = with_loop(
            &mut app,
            client.call_raw(
                "edit_file",
                json!({
                    "path": hello,
                    "edits": [
                        { "start_line": 1, "end_line": 2, "new_text": "X\n" },
                        { "start_line": 2, "end_line": 3, "new_text": "Y\n" }
                    ]
                }),
            ),
        )
        .await?;

        let is_error = resp.get("error").is_some()
            || resp["result"]["isError"].as_bool().unwrap_or(false);
        assert!(
            is_error,
            "overlapping edits should be rejected; got: {resp}"
        );
        assert_eq!(
            fs::read_to_string(&hello)?,
            "Hello, world!\nLine two\nLine three\n",
            "file must be unchanged after rejected edit"
        );
    }

    Ok(())
}
