use crate::{editor_tx, McpCommand, TextEdit};
use anyhow::{anyhow, Result};
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::sync::oneshot;

// ── write_file ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// New file contents (full replacement)
    pub content: String,
}

pub async fn handle_write_file(params: WriteFileParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::WriteFile {
        path,
        content: params.content,
        reply: reply_tx,
    })
    .await?;
    let result = reply_rx.await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── edit_file (apply_edits) ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextEditParams {
    /// First line to replace (1-indexed, inclusive)
    pub start_line: usize,
    /// Last line to replace (1-indexed, inclusive).
    /// Use `end_line < start_line` for a pure insertion (no lines removed).
    pub end_line: usize,
    /// Replacement text. Empty string to delete lines.
    pub new_text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// List of edits to apply
    pub edits: Vec<TextEditParams>,
}

pub async fn handle_edit_file(params: EditFileParams) -> Result<CallToolResult> {
    if params.edits.is_empty() {
        anyhow::bail!("edits list is empty");
    }
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let edits: Vec<TextEdit> = params
        .edits
        .into_iter()
        .map(|e| TextEdit {
            start_line: e.start_line,
            end_line: e.end_line,
            new_text: e.new_text,
        })
        .collect();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::ApplyEdits {
        path,
        edits,
        reply: reply_tx,
    })
    .await?;
    let result = reply_rx.await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── insert_text ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InsertTextParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// Line number before which to insert (1-indexed)
    pub line: usize,
    /// Text to insert (should end with a newline)
    pub text: String,
}

pub async fn handle_insert_text(params: InsertTextParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::InsertText {
        path,
        line: params.line,
        text: params.text,
        reply: reply_tx,
    })
    .await?;
    let result = reply_rx.await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── rename_symbol ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameSymbolParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// 0-indexed line number of the symbol to rename
    pub line: usize,
    /// 0-indexed column number of the symbol to rename
    pub col: usize,
    /// New name for the symbol
    pub new_name: String,
}

pub async fn handle_rename_symbol(params: RenameSymbolParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::RenameSymbol {
        path,
        line: params.line,
        col: params.col,
        new_name: params.new_name,
        reply: reply_tx,
    })
    .await?;
    let result = reply_rx.await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    std::fs::canonicalize(&p).unwrap_or(p)
}
