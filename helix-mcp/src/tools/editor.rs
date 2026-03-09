//! Editor state tools: get_cursor, get_selections, get_viewport.

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{editor_tx, McpCommand};

// ---------------------------------------------------------------------------
// get_cursor
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct CursorStateJson {
    pub path: Option<String>,
    pub line: usize,
    pub col: usize,
    pub mode: String,
    pub selection_count: usize,
}

pub async fn handle_get_cursor() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetCursor { reply: reply_tx }).await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let state = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))?;

    let mode = match state.mode {
        crate::EditorMode::Normal => "normal",
        crate::EditorMode::Insert => "insert",
        crate::EditorMode::Select => "select",
    };
    let json = serde_json::to_string_pretty(&CursorStateJson {
        path: state.path.as_deref().and_then(|p| p.to_str()).map(|s| s.to_string()),
        line: state.line,
        col: state.col,
        mode: mode.to_string(),
        selection_count: state.selection_count,
    })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// get_selections
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetSelectionsParams {
    /// Absolute path to the file.
    pub path: String,
}

#[derive(Serialize)]
pub struct SelectionRangeJson {
    pub anchor_line: usize,
    pub anchor_col: usize,
    pub head_line: usize,
    pub head_col: usize,
    pub is_primary: bool,
    pub text: String,
}

pub async fn handle_get_selections(
    params: GetSelectionsParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::GetSelections { path, reply: reply_tx }).await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let ranges = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    let json_ranges: Vec<SelectionRangeJson> = ranges
        .into_iter()
        .map(|r| SelectionRangeJson {
            anchor_line: r.anchor_line,
            anchor_col: r.anchor_col,
            head_line: r.head_line,
            head_col: r.head_col,
            is_primary: r.is_primary,
            text: r.text,
        })
        .collect();
    let json = serde_json::to_string_pretty(&json_ranges)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// get_viewport
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetViewportParams {
    /// Absolute path to the file.
    pub path: String,
}

#[derive(Serialize)]
pub struct ViewportInfoJson {
    pub first_visible_line: usize,
    pub last_visible_line: usize,
    pub height_lines: usize,
    pub horizontal_offset: usize,
}

pub async fn handle_get_viewport(
    params: GetViewportParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::GetViewport { path, reply: reply_tx }).await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let viewport = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    let json = serde_json::to_string_pretty(&ViewportInfoJson {
        first_visible_line: viewport.first_visible_line,
        last_visible_line: viewport.last_visible_line,
        height_lines: viewport.height_lines,
        horizontal_offset: viewport.horizontal_offset,
    })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}
