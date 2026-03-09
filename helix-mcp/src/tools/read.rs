use crate::{editor_tx, McpCommand};
use anyhow::Result;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::sync::oneshot;

// ── read_file ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
}

pub async fn handle_read_file(params: ReadFileParams) -> Result<CallToolResult> {
    let path = resolve_path(&params.path);

    if let Some(tx) = editor_tx() {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(McpCommand::ReadFile { path, reply: reply_tx }).await?;
        let content = reply_rx.await??;
        let line_count = content.lines().count();
        let json = serde_json::json!({
            "content": [{ "type": "text", "text": content }],
            "metadata": { "from_buffer": true, "line_count": line_count }
        });
        Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
    } else {
        // No editor channel — fall back to disk
        let content = std::fs::read_to_string(&params.path)?;
        let line_count = content.lines().count();
        let json = serde_json::json!({
            "content": [{ "type": "text", "text": content }],
            "metadata": { "from_buffer": false, "line_count": line_count }
        });
        Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
    }
}

// ── read_range ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadRangeParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// First line to read, 0-indexed, inclusive
    pub start_line: usize,
    /// Last line to read, 0-indexed, inclusive
    pub end_line: usize,
}

pub async fn handle_read_range(params: ReadRangeParams) -> Result<CallToolResult> {
    if params.end_line < params.start_line {
        anyhow::bail!(
            "end_line ({}) must be >= start_line ({})",
            params.end_line,
            params.start_line
        );
    }

    let path = resolve_path(&params.path);

    if let Some(tx) = editor_tx() {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(McpCommand::ReadRange {
            path,
            start_line: params.start_line,
            end_line: params.end_line,
            reply: reply_tx,
        })
        .await?;
        let raw = reply_rx.await??;
        let numbered = add_line_numbers(&raw, params.start_line);
        let json = serde_json::json!({
            "content": [{ "type": "text", "text": numbered }],
            "metadata": {
                "start_line": params.start_line,
                "end_line": params.end_line,
                "from_buffer": true,
            }
        });
        Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
    } else {
        // Fallback: read from disk and slice
        let full = std::fs::read_to_string(&params.path)?;
        let lines: Vec<&str> = full.lines().collect();
        let start = params.start_line.min(lines.len());
        let end = (params.end_line + 1).min(lines.len());
        if start > end {
            anyhow::bail!("start_line out of bounds");
        }
        let raw = lines[start..end].join("\n");
        let numbered = add_line_numbers(&raw, start);
        let json = serde_json::json!({
            "content": [{ "type": "text", "text": numbered }],
            "metadata": {
                "start_line": start,
                "end_line": end.saturating_sub(1),
                "from_buffer": false,
            }
        });
        Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
    }
}

// ── get_open_buffers ─────────────────────────────────────────────────────────

pub async fn handle_get_open_buffers() -> Result<CallToolResult> {
    let Some(tx) = editor_tx() else {
        let json = serde_json::json!({ "buffers": [] });
        return Ok(CallToolResult::success(vec![Content::text(json.to_string())]));
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::GetOpenBuffers { reply: reply_tx }).await?;
    let buffers = reply_rx.await?;

    let json_buffers: Vec<serde_json::Value> = buffers
        .into_iter()
        .map(|b| {
            serde_json::json!({
                "path": b.path.to_string_lossy(),
                "language": b.language,
                "is_modified": b.is_modified,
                "line_count": b.line_count,
                "lsp_servers": b.lsp_servers,
            })
        })
        .collect();

    let json = serde_json::json!({ "buffers": json_buffers });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Resolve path: canonicalize if the file exists, otherwise keep as-is.
fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// Prefix each line with its 0-indexed line number.
fn add_line_numbers(text: &str, start_line: usize) -> String {
    text.lines()
        .enumerate()
        .map(|(i, line)| format!("{}: {}", start_line + i, line))
        .collect::<Vec<_>>()
        .join("\n")
}
