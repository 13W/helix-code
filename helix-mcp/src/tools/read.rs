use crate::{editor_tx, fetch_file_content, truncate_to_char_boundary, McpCommand, MAX_INLINE_BYTES};
use anyhow::Result;
use rmcp::model::{CallToolResult, Content, RawResource};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::sync::oneshot;

use super::serde_lenient;

// ── read_file ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
}

pub async fn handle_read_file(params: ReadFileParams) -> Result<CallToolResult> {
    let path = resolve_path(&params.path);
    let (content, from_buffer) = fetch_file_content(path.clone()).await?;
    let line_count = content.lines().count();

    if content.len() <= MAX_INLINE_BYTES {
        let json = serde_json::json!({
            "content": [{ "type": "text", "text": content }],
            "metadata": { "from_buffer": from_buffer, "line_count": line_count }
        });
        Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
    } else {
        let truncated = truncate_to_char_boundary(&content, MAX_INLINE_BYTES);
        let uri = format!("helix://buffer{}", path.display());
        let json = serde_json::json!({
            "content": [{ "type": "text", "text": truncated }],
            "metadata": {
                "from_buffer": from_buffer,
                "line_count": line_count,
                "truncated": true,
                "total_bytes": content.len(),
                "uri": uri,
            }
        });
        let resource_link = Content::resource_link(RawResource {
            uri: uri.clone(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            title: None,
            description: Some(format!(
                "Full file content ({} bytes). Use read_resource to fetch.",
                content.len()
            )),
            mime_type: Some("text/plain".into()),
            size: None,
            icons: None,
            meta: None,
        });
        Ok(CallToolResult::success(vec![
            Content::text(json.to_string()),
            resource_link,
        ]))
    }
}

// ── read_range ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadRangeParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// First line to read, 0-indexed, inclusive
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub start_line: usize,
    /// Last line to read, 0-indexed, inclusive
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
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
/// Prefix each line with its 1-indexed line number.
/// The `start_line` parameter is 0-indexed (matching the read_range parameter),
/// but the output labels are 1-indexed to match `edit_file` and `insert_text`
/// conventions — so the label shown for a line can be passed directly as
/// `start_line` to `edit_file`.
fn add_line_numbers(text: &str, start_line: usize) -> String {
    text.lines()
        .enumerate()
        .map(|(i, line)| format!("{}: {}", start_line + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}
